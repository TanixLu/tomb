use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use dashmap::DashMap;
use lazy_static::lazy_static;
use regex::Regex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const DEBUG: bool = false;

pub type MailBox = DashMap<String, (Instant, String)>;

lazy_static! {
    pub static ref CODE_REGEX1: Regex = Regex::new(r"Please enter this verification code to get started on X:\s*(\d{6})\s*Verification codes expire after two hours.").unwrap();
}

lazy_static! {
    pub static ref CODE_REGEX2: Regex =
        Regex::new(r"following single-use code.\s*([a-z0-9]{8})\s*If this was").unwrap();
}

fn extract_pattern(regex: &Regex, data: &str) -> Option<String> {
    regex
        .captures(data)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Mail {
    pub from: String,
    pub to: Vec<String>,
    pub data: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum State {
    Fresh,
    Greeted,
    ReceivingRcpt(Mail),
    ReceivingData(Mail),
    Received(Mail),
}

struct StateMachine {
    state: State,
    ehlo_greeting: String,
}

/// An state machine capable of handling SMTP commands
/// for receiving mail.
/// Use handle_smtp() to handle a single command.
/// The return value from handle_smtp() is the response
/// that should be sent back to the client.
impl StateMachine {
    const OH_HAI: &'static [u8] = b"220 tomb\n";
    const KK: &'static [u8] = b"250 Ok\n";
    const AUTH_OK: &'static [u8] = b"235 Ok\n";
    const SEND_DATA_PLZ: &'static [u8] = b"354 End data with <CR><LF>.<CR><LF>\n";
    const KTHXBYE: &'static [u8] = b"221 Bye\n";
    const HOLD_YOUR_HORSES: &'static [u8] = &[];

    pub fn new(domain: impl AsRef<str>) -> Self {
        let domain = domain.as_ref();
        let ehlo_greeting = format!("250-{domain} Hello {domain}\n250 AUTH PLAIN LOGIN\n");
        Self {
            state: State::Fresh,
            ehlo_greeting,
        }
    }

    /// Handles a single SMTP command and returns a proper SMTP response
    pub fn handle_smtp(&mut self, raw_msg: &str) -> Result<&[u8]> {
        let mut msg = raw_msg.split_whitespace();
        let command = msg.next().context("received empty command")?.to_lowercase();
        let state = std::mem::replace(&mut self.state, State::Fresh);
        match (command.as_str(), state) {
            ("ehlo", State::Fresh) => {
                self.state = State::Greeted;
                Ok(self.ehlo_greeting.as_bytes())
            }
            ("helo", State::Fresh) => {
                self.state = State::Greeted;
                Ok(StateMachine::KK)
            }
            ("noop", _) | ("help", _) | ("info", _) | ("vrfy", _) | ("expn", _) => {
                Ok(StateMachine::KK)
            }
            ("rset", _) => {
                self.state = State::Fresh;
                Ok(StateMachine::KK)
            }
            ("auth", _) => Ok(StateMachine::AUTH_OK),
            ("mail", State::Greeted) => {
                let from = msg.next().context("received empty MAIL")?;
                let from = from
                    .strip_prefix("FROM:<")
                    .context("received incorrect MAIL")?;
                let from = from.strip_suffix('>').context("received incorrect MAIL")?;
                let from = from.to_lowercase();
                self.state = State::ReceivingRcpt(Mail {
                    from: from.to_string(),
                    ..Default::default()
                });
                Ok(StateMachine::KK)
            }
            ("rcpt", State::ReceivingRcpt(mut mail)) => {
                let to = msg.next().context("received empty RCPT")?;
                let to = to.strip_prefix("TO:<").context("received incorrect RCPT")?;
                let to = to
                    .split('@')
                    .next()
                    .context("received incorrect RCPT")?
                    .to_string();
                let to = to.to_lowercase();
                mail.to.push(to);
                self.state = State::ReceivingRcpt(mail);
                Ok(StateMachine::KK)
            }
            ("data", State::ReceivingRcpt(mail)) => {
                self.state = State::ReceivingData(mail);
                Ok(StateMachine::SEND_DATA_PLZ)
            }
            ("quit", State::ReceivingData(mail)) => {
                self.state = State::Received(mail);
                Ok(StateMachine::KTHXBYE)
            }
            ("quit", _) => Ok(StateMachine::KTHXBYE),
            (_, State::ReceivingData(mut mail)) => {
                let resp = if raw_msg.ends_with("\r\n.\r\n") {
                    StateMachine::KK
                } else {
                    StateMachine::HOLD_YOUR_HORSES
                };
                mail.data += raw_msg;
                self.state = State::ReceivingData(mail);
                Ok(resp)
            }
            _ => anyhow::bail!(
                "Unexpected message received in state {:?}: {raw_msg}",
                self.state
            ),
        }
    }
}

/// SMTP server, which handles user connections
/// and replicates received messages to the database.
pub struct Server {
    stream: tokio::net::TcpStream,
    state_machine: StateMachine,
    mailbox: Arc<MailBox>,
}

impl Server {
    /// Creates a new server from a connected stream
    pub fn new(
        domain: impl AsRef<str>,
        stream: tokio::net::TcpStream,
        mailbox: Arc<MailBox>,
    ) -> Self {
        Self {
            stream,
            state_machine: StateMachine::new(domain),
            mailbox,
        }
    }

    /// Runs the server loop, accepting and handling SMTP commands
    pub async fn serve(mut self) -> Result<()> {
        self.greet().await?;

        let mut buf = vec![0; 65536];
        loop {
            let n = self.stream.read(&mut buf).await?;

            if n == 0 {
                self.state_machine.handle_smtp("quit").ok();
                break;
            }
            let msg = std::str::from_utf8(&buf[0..n])?;
            let response = self.state_machine.handle_smtp(msg)?;
            if response != StateMachine::HOLD_YOUR_HORSES {
                self.stream.write_all(response).await?;
            }
            if response == StateMachine::KTHXBYE {
                break;
            }
        }
        match self.state_machine.state {
            State::Received(mail) | State::ReceivingData(mail) => {
                if !mail.to.is_empty() {
                    if DEBUG {
                        let mut from = String::new();
                        let mut to = String::new();
                        let mut subject = String::new();
                        let mut date = String::new();
                        for line in mail.data.lines() {
                            let line = line.trim();
                            if line.starts_with("Date:") {
                                date = line.to_string();
                            } else if line.starts_with("From:") {
                                from = line.to_string();
                            } else if line.starts_with("To:") {
                                to = line.to_string();
                            } else if line.starts_with("Subject:") {
                                subject = line.to_string();
                            }
                        }
                        let (email_type, code) =
                            if let Some(code) = extract_pattern(&CODE_REGEX1, &mail.data) {
                                let mail_to = mail.to.into_iter().next().unwrap();
                                self.mailbox.insert(mail_to, (Instant::now(), code.clone()));
                                ("code1".to_string(), code)
                            } else if let Some(code) = extract_pattern(&CODE_REGEX2, &mail.data) {
                                let mail_to = mail.to.into_iter().next().unwrap();
                                self.mailbox.insert(mail_to, (Instant::now(), code.clone()));
                                ("code2".to_string(), code)
                            } else {
                                let mail_to = mail.to.into_iter().next().unwrap();
                                self.mailbox.insert(mail_to, (Instant::now(), mail.data));
                                ("no_code".to_string(), "no_code".to_string())
                            };
                        println!("{from} {to} {subject} {date} {email_type} {code}");
                    } else {
                        if let Some(code) = extract_pattern(&CODE_REGEX1, &mail.data) {
                            let mail_to = mail.to.into_iter().next().unwrap();
                            self.mailbox.insert(mail_to, (Instant::now(), code.clone()));
                        } else if let Some(code) = extract_pattern(&CODE_REGEX2, &mail.data) {
                            let mail_to = mail.to.into_iter().next().unwrap();
                            self.mailbox.insert(mail_to, (Instant::now(), code.clone()));
                        }
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Sends the initial SMTP greeting
    async fn greet(&mut self) -> Result<()> {
        self.stream
            .write_all(StateMachine::OH_HAI)
            .await
            .map_err(|e| e.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_regular_flow() {
        let mut sm = StateMachine::new("dummy");
        assert_eq!(sm.state, State::Fresh);
        sm.handle_smtp("HELO localhost").unwrap();
        assert_eq!(sm.state, State::Greeted);
        sm.handle_smtp("MAIL FROM:<local@example.com>").unwrap();
        assert!(matches!(sm.state, State::ReceivingRcpt(_)));
        sm.handle_smtp("RCPT TO:<a@localhost.com>").unwrap();
        assert!(matches!(sm.state, State::ReceivingRcpt(_)));
        sm.handle_smtp("RCPT TO:<b@localhost.com>").unwrap();
        assert!(matches!(sm.state, State::ReceivingRcpt(_)));
        sm.handle_smtp("DATA hello world\n").unwrap();
        assert!(matches!(sm.state, State::ReceivingData(_)));
        sm.handle_smtp("DATA hello world2\n").unwrap();
        assert!(matches!(sm.state, State::ReceivingData(_)));
        sm.handle_smtp("QUIT").unwrap();
        assert!(matches!(sm.state, State::Received(_)));
    }

    #[test]
    fn test_no_greeting() {
        let mut sm = StateMachine::new("dummy");
        assert_eq!(sm.state, State::Fresh);
        for command in [
            "MAIL FROM:<local@example.com>",
            "RCPT TO:<local@example.com>",
            "DATA hey",
            "GARBAGE",
        ] {
            assert!(sm.handle_smtp(command).is_err());
        }
    }
}
