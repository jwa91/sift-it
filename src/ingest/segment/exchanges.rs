//! Exchanges: a conversation's messages, in order, joined into what was asked and what the agent answered. An exchange
//! starts with what the user says after the agent has answered, and takes what the agent says until the user speaks
//! again. Its text names who speaks wherever that changes ("User: …", "Claude: …"), so a passage of it reads as the
//! conversation. The text cutter (passages.rs) cuts a long one, and each part after the first is read with what was
//! asked.

/// A message: the user's or the agent's, when it was said (seconds since 1970), and what was said.
#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub user: bool,
    pub time: f64,
    pub text: String,
}

/// An exchange as text, with when each of its messages was said.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Exchange {
    pub text: String,
    /// The line (from 1) each message starts at, with its time, in order.
    pub starts: Vec<(usize, f64)>,
    /// What was asked, as the context a part of a long exchange is read with ("User: …", about a line).
    pub asked: String,
}

impl Exchange {
    /// When a line (from 1) of it was said: its message's time.
    pub fn time(&self, line: usize) -> f64 {
        let at = self.starts.partition_point(|(start, _)| *start <= line);
        self.starts.get(at.saturating_sub(1)).map_or(0.0, |(_, time)| *time)
    }
}

/// What was asked is read as about this many characters: a line of it.
const ASKED_CHARS: usize = 200;

/// Joins a conversation's messages into exchanges as they come.
pub struct Exchanges {
    /// The agent's name, as its messages are labelled.
    agent: &'static str,
    open: Option<Exchange>,
    /// Who said the last message.
    user: bool,
}

impl Exchanges {
    pub fn new(agent: &'static str) -> Self {
        Self { agent, open: None, user: false }
    }

    /// Whether a message starts an exchange: the first one, or the user's after the agent answered.
    pub fn starts(&self, message: &Message) -> bool {
        !message.text.trim().is_empty() && (self.open.is_none() || (message.user && !self.user))
    }

    /// A message. Returns the exchange it ends, when the user speaks after the agent answered.
    pub fn push(&mut self, message: Message) -> Option<Exchange> {
        let text = message.text.trim();
        if text.is_empty() {
            return None;
        }
        let ended = if message.user && !self.user { self.open.take() } else { None };
        let speaker_changes = self.open.is_none() || message.user != self.user;
        let open = self.open.get_or_insert_with(Exchange::default);
        let start = if open.text.is_empty() { 1 } else { open.text.matches('\n').count() + 3 };
        if !open.text.is_empty() {
            open.text.push_str("\n\n");
        }
        if speaker_changes {
            open.text.push_str(if message.user { "User: " } else { self.agent });
            if !message.user {
                open.text.push_str(": ");
            }
        }
        open.text.push_str(text);
        open.starts.push((start, message.time));
        if message.user && open.asked.is_empty() {
            open.asked = asked(text);
        }
        self.user = message.user;
        ended
    }

    /// The end of the messages: the last exchange.
    pub fn finish(&mut self) -> Option<Exchange> {
        self.user = false;
        self.open.take()
    }
}

/// What was asked, on one line, cut at about ASKED_CHARS.
fn asked(text: &str) -> String {
    let line: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let cut = match line.char_indices().nth(ASKED_CHARS) {
        Some((end, _)) => format!("{}…", line[..end].trim_end()),
        None => line,
    };
    format!("User: {cut}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn said(user: bool, time: f64, text: &str) -> Message {
        Message { user, time, text: text.into() }
    }

    #[test]
    fn an_exchange_is_what_was_asked_and_all_that_was_answered() {
        let mut exchanges = Exchanges::new("Claude");
        let messages = [
            said(true, 10.0, "Which tent fits two?"),
            said(true, 11.0, "  And a dog.  "),
            said(false, 12.0, "Let me look."),
            said(false, 15.0, "The Trailhead 3.\nIt has room for a dog."),
            said(true, 20.0, "Thanks!"),
            said(false, 21.0, ""),
        ];
        let ended: Vec<Exchange> = messages.into_iter().filter_map(|message| exchanges.push(message)).collect();
        assert_eq!(ended.len(), 1);
        let first = &ended[0];
        assert_eq!(
            first.text,
            "User: Which tent fits two?\n\nAnd a dog.\n\nClaude: Let me look.\n\nThe Trailhead 3.\nIt has room for a dog."
        );
        assert_eq!(first.starts, [(1, 10.0), (3, 11.0), (5, 12.0), (7, 15.0)]);
        assert_eq!(first.asked, "User: Which tent fits two?");
        assert_eq!((first.time(1), first.time(4), first.time(8)), (10.0, 11.0, 15.0));
        let last = exchanges.finish().unwrap();
        assert_eq!((last.text.as_str(), last.starts.as_slice()), ("User: Thanks!", [(1, 20.0)].as_slice()));
        assert!(exchanges.finish().is_none());
    }

    #[test]
    fn what_was_asked_is_a_line() {
        let long = format!("Plan a trip\nto {}", "the coast and the hills ".repeat(20));
        let asked = asked(&long);
        assert!(asked.starts_with("User: Plan a trip to the coast") && asked.ends_with('…'));
        assert!(asked.chars().count() <= "User: ".len() + ASKED_CHARS + 1);
    }
}
