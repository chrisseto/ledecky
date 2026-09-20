//! Sending a message to a running session over its inbox socket.
//!
//! The alternative is typing into the pty, which means finding the input box on
//! screen, pasting, and watching the box let go again to know it landed. This
//! needs none of that: the session reads the message between tool calls, so a
//! dialog holding the keyboard does not block delivery, and there is no screen
//! to misread.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::json;

/// How long to wait on the socket before giving up.
///
/// NB: not a `Settings` knob, unlike the waits the agent plumbing makes. Nothing
/// is being waited *out* here — a local socket either accepts immediately or is
/// not there — so this is a backstop against a hung peer, not a timing choice.
const TIMEOUT: Duration = Duration::from_secs(5);

/// Where a session takes messages, as reported by its `SessionStart` hook.
#[derive(Clone, Debug)]
pub struct Inbox {
    pub socket: String,
    pub token: String,
}

/// Sends `text` to the session as one message.
///
/// NB: what arrives is framed to the agent as a message from another session,
/// not as something the user typed — it cannot answer a pending permission
/// prompt, and a command in it is read as text. That suits a relayed review; it
/// is why the opening task goes in on the command line instead.
pub fn send(inbox: &Inbox, text: &str) -> Result<()> {
    let Inbox { socket, token } = inbox;

    // NB: connect only now that the text is in hand. The session closes a
    // connection that has not sent a complete line soon after it opens.
    let mut stream =
        UnixStream::connect(socket).with_context(|| format!("connecting to {socket}"))?;
    stream.set_write_timeout(Some(TIMEOUT))?;

    let mut payload = String::new();
    // Optional on Linux, and the only thing that identifies us on platforms
    // where it is not.
    if !token.is_empty() {
        payload.push_str(&json!({ "type": "auth", "token": token }).to_string());
        payload.push('\n');
    }
    payload.push_str(
        &json!({ "type": "user", "message": { "role": "user", "content": text } }).to_string(),
    );
    payload.push('\n');

    stream
        .write_all(payload.as_bytes())
        .with_context(|| format!("writing to {socket}"))?;
    stream
        .flush()
        .with_context(|| format!("flushing {socket}"))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;

    /// Serves one connection and hands back every line it was sent.
    fn collect(listener: UnixListener) -> std::thread::JoinHandle<Vec<String>> {
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            BufReader::new(stream)
                .lines()
                .map_while(Result::ok)
                .collect()
        })
    }

    fn socket_path(name: &str) -> PathBuf {
        // NB: the system temp dir, not the card's. A unix socket path is capped
        // near 100 bytes and a data directory is nowhere near short enough.
        let path = std::env::temp_dir().join(format!("ledecky-{name}-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        path
    }

    #[test]
    fn a_message_goes_out_as_an_auth_line_and_a_user_message() {
        let path = socket_path("send");
        let served = collect(UnixListener::bind(&path).unwrap());

        let inbox = Inbox {
            socket: path.to_string_lossy().into_owned(),
            token: "secret-token".to_owned(),
        };
        send(&inbox, "Land it on `main`").unwrap();

        let lines = served.join().unwrap();
        assert_eq!(
            lines[0], r#"{"token":"secret-token","type":"auth"}"#,
            "the auth line comes first"
        );
        let sent: serde_json::Value = serde_json::from_str(&lines[1]).unwrap();
        assert_eq!(sent["type"], "user");
        assert_eq!(sent["message"]["role"], "user");
        assert_eq!(sent["message"]["content"], "Land it on `main`");

        let _ = std::fs::remove_file(&path);
    }

    /// Auth is optional on Linux, and an empty token is what a client that never
    /// published one reports. Sending the line anyway would be malformed.
    #[test]
    fn an_empty_token_sends_no_auth_line() {
        let path = socket_path("noauth");
        let served = collect(UnixListener::bind(&path).unwrap());

        let inbox = Inbox {
            socket: path.to_string_lossy().into_owned(),
            token: String::new(),
        };
        send(&inbox, "go").unwrap();

        let lines = served.join().unwrap();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].starts_with(r#"{"message""#));

        let _ = std::fs::remove_file(&path);
    }

    /// A session that died leaves its socket behind, so the send has to fail
    /// rather than report a message nobody received.
    #[test]
    fn a_socket_with_nothing_behind_it_is_an_error() {
        let inbox = Inbox {
            socket: socket_path("dead").to_string_lossy().into_owned(),
            token: String::new(),
        };
        assert!(send(&inbox, "anything").is_err());
    }
}
