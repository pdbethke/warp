//! Runs an ACP agent inside the driver's PTY shell session while the driver talks to it over a
//! loopback socket.
//!
//! The driver listens on `127.0.0.1`, writes the address, a one-time token, and the agent command
//! line to a launch file in a private directory, and submits
//! `"$OZ_CLI" acp-bridge --launch-file <path>` as a command in the terminal session. Nothing
//! else crosses the shell, so the agent's argv is never subject to the shell's quoting or
//! native-argument rules. The bridge subcommand connects, proves it is the expected peer by
//! sending the token, and then hands the connection to the agent as its stdin and stdout: on Unix
//! by replacing itself with the agent, elsewhere by relaying between the socket and the agent's
//! pipes. Either way the agent runs as the shell's foreground command (it inherits the shell
//! state established by environment setup commands, its launch is a block in the shared session,
//! and the terminal's interrupt and force-kill paths reach it) while its protocol stream never
//! touches the PTY.
//!
//! This requires the driver, the shell session, and the agent to share one OS instance (loopback
//! networking and the launch file are both host-local). That holds for every cloud-runner shell;
//! it would not for a sandboxed, WSL, or SSH session launched from a host Warp.
use std::io::{self, Read as _, Write as _};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow};
use async_io::Async;
use futures::FutureExt as _;
use futures::io::{AsyncRead, AsyncReadExt as _, AsyncWrite};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use warp_util::path::ShellFamily;

/// Environment variable holding the path of the Warp CLI, injected into every agent session.
const CLI_ENV_VAR: &str = warp_cli::OZ_CLI_ENV;
const LAUNCH_FILE_NAME: &str = "launch.json";
/// Upper bound on the token line the bridge sends first; anything longer is not our peer.
const MAX_TOKEN_LINE: usize = 128;
/// How long a relayed agent may keep running after the driver has closed the connection before
/// the bridge terminates it.
#[cfg_attr(unix, allow(dead_code, reason = "Unix execs the agent instead"))]
const AGENT_EXIT_GRACE: Duration = Duration::from_secs(5);

/// What the driver hands the bridge through the launch file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct BridgeLaunch {
    /// Loopback address the driver accepts on.
    pub address: SocketAddr,
    /// Presented before any protocol traffic so the driver can tell the bridge from any other
    /// local client that found the port.
    pub token: String,
    /// The agent command line.
    pub agent: Vec<String>,
}

/// Builds the shell command that launches the agent described by `launch_file` through the
/// bridge.
pub(crate) fn bridge_command(launch_file: &Path, shell_family: ShellFamily) -> String {
    let cli = match shell_family {
        ShellFamily::Posix => format!("\"${CLI_ENV_VAR}\""),
        ShellFamily::PowerShell => format!("& $env:{CLI_ENV_VAR}"),
    };
    format!(
        "{cli} acp-bridge --launch-file {}",
        shell_family.shell_escape(&launch_file.display().to_string())
    )
}

/// Worker-side entry point. Only returns on failure; on success the agent's exit status becomes
/// this process's.
pub(crate) fn run_bridge(launch_file: &Path) -> Result<()> {
    let launch = read_launch(launch_file)?;
    let (program, args) = launch
        .agent
        .split_first()
        .ok_or_else(|| anyhow!("The ACP launch file names no agent command"))?;
    let stream = connect_with_token(launch.address, &launch.token)?;
    #[cfg(unix)]
    {
        use command::unix::CommandExt as _;
        let mut command = exec_agent_command(stream, program, args)?;
        let error = command.exec();
        Err(anyhow!(error)).with_context(|| format!("Failed to exec ACP agent `{program}`"))
    }
    #[cfg(not(unix))]
    {
        let status = relay_agent(stream, program, args, AGENT_EXIT_GRACE)?;
        std::process::exit(status.code().unwrap_or(1))
    }
}

fn read_launch(launch_file: &Path) -> Result<BridgeLaunch> {
    let contents = std::fs::read_to_string(launch_file)
        .with_context(|| format!("Failed to read ACP launch file {}", launch_file.display()))?;
    serde_json::from_str(&contents)
        .with_context(|| format!("Malformed ACP launch file {}", launch_file.display()))
}

/// Connects to the driver and sends the token it expects before any protocol traffic.
fn connect_with_token(address: SocketAddr, token: &str) -> Result<TcpStream> {
    let mut stream = TcpStream::connect(address)
        .with_context(|| format!("Failed to connect to the ACP driver at {address}"))?;
    stream.set_nodelay(true)?;
    stream
        .write_all(format!("{token}\n").as_bytes())
        .context("Failed to send the ACP token")?;
    Ok(stream)
}

/// The agent command with the socket as its stdin and stdout. Stderr is inherited, so agent
/// diagnostics land in the terminal block.
#[cfg(unix)]
fn exec_agent_command(
    stream: TcpStream,
    program: &str,
    args: &[String],
) -> Result<command::blocking::Command> {
    use std::os::fd::OwnedFd;
    use std::process::Stdio;

    let stdin = stream
        .try_clone()
        .context("Failed to clone the ACP socket for stdin")?;
    let mut command = command::blocking::Command::new(program);
    command
        .args(args)
        .stdin(Stdio::from(OwnedFd::from(stdin)))
        .stdout(Stdio::from(OwnedFd::from(stream)));
    Ok(command)
}

/// Runs the agent with piped stdin/stdout and copies bytes between those pipes and the socket
/// until the agent exits. Used where the agent cannot simply inherit the socket.
///
/// Once the driver closes its side the agent sees EOF on stdin; if it is still running after
/// `exit_grace` it is terminated so the terminal block cannot hang on an agent that ignores EOF.
#[cfg_attr(
    unix,
    allow(dead_code, reason = "Unix execs the agent instead; kept testable")
)]
fn relay_agent(
    stream: TcpStream,
    program: &str,
    args: &[String],
    exit_grace: Duration,
) -> Result<std::process::ExitStatus> {
    use std::process::Stdio;
    use std::sync::mpsc;

    // Agents installed through npm are `.cmd` shims on Windows. `CreateProcess` does not consult
    // `PATHEXT`, so look the program up the way the shell would; std then runs a `.cmd`/`.bat`
    // path through `cmd.exe` with the appropriate quoting.
    let program_path = warp_util::path::resolve_executable(program)
        .ok_or_else(|| anyhow!("ACP agent `{program}` was not found on PATH"))?;
    let mut child = command::blocking::Command::new(program_path.as_ref())
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("Failed to start ACP agent `{program}`"))?;
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle as _;
        // Ties the agent and anything it spawns (the shim's `node`, for instance) to this
        // process: when the bridge exits for any reason, including being killed, the job closes
        // and the whole agent tree goes with it.
        if let Err(error) = command::windows::JobObject::new()
            .assign_process(child.as_raw_handle() as isize)
            .kill_children_on_close()
            .create()
        {
            eprintln!("acp-bridge: the agent will outlive the bridge if killed: {error:#}");
        }
    }
    let mut agent_stdin = child.stdin.take().context("agent has no stdin")?;
    let mut agent_stdout = child.stdout.take().context("agent has no stdout")?;

    let mut socket_reader = stream.try_clone()?;
    let (driver_closed_tx, driver_closed_rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("acp-bridge-to-agent".into())
        .spawn(move || {
            // EOF from the driver (or a failure) closes the agent's stdin by dropping it.
            let _ = io::copy(&mut socket_reader, &mut agent_stdin);
            drop(agent_stdin);
            let _ = driver_closed_tx.send(());
        })?;
    let mut socket_writer = stream.try_clone()?;
    let (agent_drained_tx, agent_drained_rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("acp-bridge-from-agent".into())
        .spawn(move || {
            let mut buffer = [0u8; 8192];
            loop {
                match agent_stdout.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if socket_writer.write_all(&buffer[..n]).is_err() {
                            break;
                        }
                    }
                }
            }
            let _ = socket_writer.shutdown(Shutdown::Write);
            let _ = agent_drained_tx.send(());
        })?;

    let status = wait_with_exit_grace(&mut child, &driver_closed_rx, exit_grace)?;
    // Deliver whatever the agent wrote before exiting, but do not wait on a stdout pipe that a
    // surviving grandchild may still hold open. The threads are not joined: the socket pump
    // blocks until the driver closes its side, which may be long after the agent is gone.
    let _ = agent_drained_rx.recv_timeout(exit_grace);
    let _ = stream.shutdown(Shutdown::Both);
    Ok(status)
}

/// Waits for `child` to exit, killing it if it is still running `exit_grace` after
/// `driver_closed` fires.
#[cfg_attr(
    unix,
    allow(dead_code, reason = "Unix execs the agent instead; kept testable")
)]
fn wait_with_exit_grace(
    child: &mut std::process::Child,
    driver_closed: &std::sync::mpsc::Receiver<()>,
    exit_grace: Duration,
) -> io::Result<std::process::ExitStatus> {
    use std::sync::mpsc::RecvTimeoutError;

    use instant::Instant;

    const POLL_INTERVAL: Duration = Duration::from_millis(50);

    let mut kill_at = None;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        match kill_at {
            None => match driver_closed.recv_timeout(POLL_INTERVAL) {
                Ok(()) | Err(RecvTimeoutError::Disconnected) => {
                    kill_at = Some(Instant::now() + exit_grace);
                }
                Err(RecvTimeoutError::Timeout) => {}
            },
            Some(deadline) if Instant::now() >= deadline => {
                eprintln!("acp-bridge: agent did not exit within {exit_grace:?} of EOF; killing");
                child.kill()?;
                return child.wait();
            }
            Some(_) => std::thread::sleep(POLL_INTERVAL),
        }
    }
}

/// Driver-side listener for one bridged agent.
pub(crate) struct BridgeListener {
    /// Holds the private directory the launch file lives in; removed with the listener.
    _dir: tempfile::TempDir,
    token: String,
    launch_file: PathBuf,
    address: SocketAddr,
    listener: Async<std::net::TcpListener>,
}

impl BridgeListener {
    /// Listens on an ephemeral loopback port and writes the launch file, with a fresh token, for
    /// the agent command `program` `args`.
    pub(crate) fn bind(program: &str, args: &[String]) -> Result<Self> {
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .context("Failed to bind a loopback port for the ACP bridge")?;
        let address = listener.local_addr()?;
        let listener = Async::new(listener).context("Failed to register the ACP listener")?;

        // The token is only as private as this directory. `tempfile` creates it mode 0700 on
        // Unix; on Windows it inherits the ACL of `%TEMP%`, which is per-user by default.
        let dir = tempfile::Builder::new()
            .prefix("oz-acp-")
            .tempdir()
            .context("Failed to create a directory for the ACP launch file")?;
        let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        let launch = BridgeLaunch {
            address,
            token: token.clone(),
            agent: std::iter::once(program.to_owned())
                .chain(args.iter().cloned())
                .collect(),
        };
        let launch_file = dir.path().join(LAUNCH_FILE_NAME);
        std::fs::write(&launch_file, serde_json::to_vec(&launch)?).with_context(|| {
            format!("Failed to write ACP launch file {}", launch_file.display())
        })?;
        Ok(Self {
            _dir: dir,
            token,
            launch_file,
            address,
            listener,
        })
    }

    pub(crate) fn launch_file(&self) -> &Path {
        &self.launch_file
    }

    /// Waits for the bridge to connect and present the token, ignoring other local clients, and
    /// returns the agent's stdout (our reader) and stdin (our writer) ends of the socket.
    pub(crate) async fn accept(&self, timeout: Duration) -> Result<(BridgeReader, BridgeWriter)> {
        let deadline = warpui::r#async::Timer::after(timeout).fuse();
        futures::pin_mut!(deadline);
        loop {
            let accept = self.listener.accept().fuse();
            futures::pin_mut!(accept);
            let stream = futures::select! {
                accepted = accept => {
                    let (stream, _) =
                        accepted.context("Failed to accept the ACP bridge connection")?;
                    stream
                }
                _ = deadline => return Err(anyhow!(
                    "The ACP agent did not connect to {} within {timeout:?}",
                    self.address
                )),
            };
            let presented = {
                let token_line = read_token_line(&stream).fuse();
                futures::pin_mut!(token_line);
                futures::select! {
                    presented = token_line => presented,
                    _ = deadline => return Err(anyhow!(
                        "The ACP agent did not present its token within {timeout:?}"
                    )),
                }
            };
            match presented {
                Ok(presented) if presented == self.token => {
                    let stream = Arc::new(stream);
                    return Ok((BridgeReader(stream.clone()), BridgeWriter(stream)));
                }
                Ok(_) => log::warn!("Rejected an ACP bridge connection with the wrong token"),
                Err(error) => log::warn!("Rejected an ACP bridge connection: {error}"),
            }
        }
    }
}

/// Reads the newline-terminated token the bridge sends first, one byte at a time so nothing of
/// the protocol stream that follows is consumed.
async fn read_token_line(stream: &Async<TcpStream>) -> Result<String> {
    let mut stream = stream;
    let mut line = Vec::with_capacity(64);
    let mut byte = [0u8; 1];
    loop {
        stream.read_exact(&mut byte).await?;
        if byte[0] == b'\n' {
            return Ok(String::from_utf8(line)?.trim_end().to_owned());
        }
        line.push(byte[0]);
        if line.len() > MAX_TOKEN_LINE {
            anyhow::bail!("token line too long");
        }
    }
}

/// The agent's stdout.
pub(crate) struct BridgeReader(Arc<Async<TcpStream>>);

impl AsyncRead for BridgeReader {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut &*self.0).poll_read(cx, buf)
    }
}

/// The agent's stdin. Closing it shuts down the socket's write side, which the agent sees as
/// EOF; dropping it without closing leaves the agent waiting for more input.
pub(crate) struct BridgeWriter(Arc<Async<TcpStream>>);

impl AsyncWrite for BridgeWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut &*self.0).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut &*self.0).poll_flush(cx)
    }

    fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(self.0.get_ref().shutdown(Shutdown::Write))
    }
}

#[cfg(test)]
#[path = "bridge_tests.rs"]
mod tests;
