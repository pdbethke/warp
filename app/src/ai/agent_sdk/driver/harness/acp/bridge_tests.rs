use std::io::Write as _;
use std::net::TcpStream;
use std::path::Path;
use std::time::Duration;

use futures::executor::block_on;
use futures::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use instant::Instant;
use warp_util::path::ShellFamily;

use super::{
    BridgeListener, BridgeReader, BridgeWriter, bridge_command, connect_with_token, read_launch,
};

#[test]
fn bridge_command_reads_the_cli_from_the_environment_and_only_names_the_launch_file() {
    let command = bridge_command(Path::new("/tmp/oz-acp x/launch.json"), ShellFamily::Posix);
    assert_eq!(
        command,
        "\"$OZ_CLI\" acp-bridge --launch-file /tmp/oz-acp\\ x/launch.json"
    );

    let command = bridge_command(
        Path::new(r"C:\Temp\oz-acp-x\launch.json"),
        ShellFamily::PowerShell,
    );
    assert_eq!(
        command,
        r"& $env:OZ_CLI acp-bridge --launch-file C:\Temp\oz-acp-x\launch.json"
    );
}

#[test]
fn launch_file_carries_the_agent_command() {
    let listener =
        BridgeListener::bind("npx", &["-y".into(), "@zed-industries/codex-acp".into()]).unwrap();
    let launch = read_launch(listener.launch_file()).unwrap();
    assert_eq!(launch.agent, ["npx", "-y", "@zed-industries/codex-acp"]);
    assert_eq!(launch.address, listener.address);
    assert!(!launch.token.is_empty());
}

/// Does what the bridge subcommand does before handing off to the agent.
fn connect(listener: &BridgeListener) -> TcpStream {
    let launch = read_launch(listener.launch_file()).unwrap();
    connect_with_token(launch.address, &launch.token).unwrap()
}

/// A command that echoes stdin to stdout and exits on EOF. `findstr` emits CRLF, so callers
/// compare trimmed lines.
fn echo_command() -> (&'static str, Vec<String>) {
    if cfg!(windows) {
        ("findstr", vec!["/r".into(), "^".into()])
    } else {
        ("cat", vec![])
    }
}

/// A command that never reads stdin and runs well past any test's patience.
fn ignores_eof_command() -> (&'static str, Vec<String>) {
    if cfg!(windows) {
        ("ping", vec!["-n".into(), "60".into(), "127.0.0.1".into()])
    } else {
        ("sh", vec!["-c".into(), "exec sleep 60".into()])
    }
}

/// A command that exits immediately without touching stdin.
fn exits_immediately_command() -> (&'static str, Vec<String>) {
    if cfg!(windows) {
        ("cmd", vec!["/c".into(), "exit".into(), "0".into()])
    } else {
        ("true", vec![])
    }
}

/// Drives one echo round trip and an EOF-triggered shutdown through the accepted halves.
fn assert_echo_then_eof(reader: BridgeReader, mut writer: BridgeWriter) {
    let mut reader = BufReader::new(reader);
    block_on(async {
        writer.write_all(b"{\"jsonrpc\":\"2.0\"}\n").await.unwrap();
        writer.flush().await.unwrap();
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        assert_eq!(line.trim_end(), "{\"jsonrpc\":\"2.0\"}");

        // Closing our writer shuts down the socket's write side, which is EOF on the agent's
        // stdin; merely dropping the half would leave the agent waiting.
        writer.close().await.unwrap();
        let mut rest = String::new();
        reader.read_line(&mut rest).await.unwrap();
        assert!(rest.is_empty(), "agent should close stdout after EOF");
    });
}

#[cfg(unix)]
#[test]
fn exec_style_agent_talks_to_the_driver_over_the_socket() {
    let listener = BridgeListener::bind("cat", &[]).unwrap();
    // Stand in for the exec'd agent: spawn `cat` with the socket as its stdio.
    let stream = connect(&listener);
    let mut agent = super::exec_agent_command(stream, "cat", &[])
        .unwrap()
        .spawn()
        .unwrap();

    let (reader, writer) = block_on(listener.accept(Duration::from_secs(10))).unwrap();
    assert_echo_then_eof(reader, writer);
    assert!(agent.wait().unwrap().success());
}

#[test]
fn relayed_agent_talks_to_the_driver_over_the_socket() {
    let (program, args) = echo_command();
    let listener = BridgeListener::bind(program, &args).unwrap();
    let stream = connect(&listener);
    let relay = std::thread::spawn(move || {
        super::relay_agent(stream, program, &args, Duration::from_secs(10))
    });

    let (reader, writer) = block_on(listener.accept(Duration::from_secs(10))).unwrap();
    assert_echo_then_eof(reader, writer);
    assert!(relay.join().unwrap().unwrap().success());
}

#[test]
fn relay_kills_an_agent_that_ignores_eof_after_the_grace_period() {
    let (program, args) = ignores_eof_command();
    let listener = BridgeListener::bind(program, &args).unwrap();
    let stream = connect(&listener);
    let grace = Duration::from_millis(200);
    let relay = std::thread::spawn(move || super::relay_agent(stream, program, &args, grace));

    let (reader, mut writer) = block_on(listener.accept(Duration::from_secs(10))).unwrap();
    let closed_at = Instant::now();
    block_on(writer.close()).unwrap();
    let status = relay.join().unwrap().unwrap();
    assert!(!status.success(), "agent should have been killed: {status}");
    assert!(closed_at.elapsed() < Duration::from_secs(10));
    drop(reader);
}

#[test]
fn relay_finishes_when_the_agent_exits_before_the_driver_closes_the_socket() {
    let (program, args) = exits_immediately_command();
    let listener = BridgeListener::bind(program, &args).unwrap();
    let stream = connect(&listener);
    let relay = std::thread::spawn(move || {
        super::relay_agent(stream, program, &args, Duration::from_secs(10))
    });

    // The driver keeps both halves open; the relay must still return once the agent is gone.
    let (reader, writer) = block_on(listener.accept(Duration::from_secs(10))).unwrap();
    let status = relay.join().unwrap().unwrap();
    assert!(status.success(), "{status}");
    drop((reader, writer));
}

#[test]
fn connections_without_the_token_are_ignored_until_the_real_bridge_arrives() {
    let listener = BridgeListener::bind("cat", &[]).unwrap();
    let mut impostor = TcpStream::connect(listener.address).unwrap();
    impostor.write_all(b"not-the-token\n").unwrap();
    let genuine = connect(&listener);

    let (reader, mut writer) = block_on(listener.accept(Duration::from_secs(10))).unwrap();
    // Prove we are talking to the genuine peer: it sees what we write, the impostor does not.
    block_on(async {
        writer.write_all(b"ping\n").await.unwrap();
        writer.flush().await.unwrap();
    });
    let mut genuine_reader = std::io::BufReader::new(&genuine);
    let mut line = String::new();
    std::io::BufRead::read_line(&mut genuine_reader, &mut line).unwrap();
    assert_eq!(line, "ping\n");
    drop(reader);
}

#[test]
fn accept_times_out_when_no_agent_connects() {
    let listener = BridgeListener::bind("cat", &[]).unwrap();
    let Err(error) = block_on(listener.accept(Duration::from_millis(100))) else {
        panic!("accept should time out without a client");
    };
    assert!(error.to_string().contains("did not connect"), "{error}");
}

#[test]
fn bridge_rejects_a_launch_file_without_an_agent_command() {
    let dir = tempfile::tempdir().unwrap();
    let launch_file = dir.path().join("launch.json");
    std::fs::write(
        &launch_file,
        r#"{"address":"127.0.0.1:1","token":"t","agent":[]}"#,
    )
    .unwrap();
    let error = super::run_bridge(&launch_file).unwrap_err();
    assert!(error.to_string().contains("no agent command"), "{error}");
}
