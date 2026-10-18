//! How files are read (`reading`): the cascade of reading values, from everything down to a kind, a file type and the
//! rules for a path, and the depth a file is read at.

use super::Pattern;
use crate::ingest::discover::crawl::extension_of;
use protocol::Kind;
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::path::Path;

/// The reading values: each is a field of Values, where it's optional (set at this step of the cascade or not), and
/// of Settings, where it's what a file is read with.
macro_rules! reading_values {
    ($($(#[$doc:meta])* $field:ident: $type:ty = $default:expr,)*) => {
        /// Reading values set at one step of the cascade (see Reading).
        #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
        #[serde(default, deny_unknown_fields)]
        pub struct Values {
            $($(#[$doc])* #[serde(skip_serializing_if = "Option::is_none")] pub $field: Option<$type>,)*
        }

        /// The reading values a file is read with.
        #[derive(Debug, Clone, PartialEq)]
        pub struct Settings {
            $($(#[$doc])* pub $field: $type,)*
        }

        impl Settings {
            /// Where the cascade starts: a value left out of config.json everywhere is this.
            pub const BASE: Settings = Settings { $($field: $default,)* };

            fn apply(&mut self, values: &Values) {
                $(if let Some(value) = &values.$field { self.$field = value.clone(); })*
            }
        }

        impl Values {
            /// All of them, at the start of the cascade.
            fn base() -> Self {
                Self { $($field: Some($default),)* }
            }

            /// These values, with those set in `over` in their place.
            fn overlay(&mut self, over: Values) {
                $(if over.$field.is_some() { self.$field = over.$field; })*
            }
        }
    };
}

reading_values! {
    /// How deep a file is read (see Depth).
    depth: Depth = Depth::Speech,
    /// The tokens the model sees a picture with: a photo reads well at 140, a video frame at 70.
    tokens: u32 = 140,
    /// The tokens for a picture with text on it: a screenshot, a scanned page. Small things are dense in it.
    text_tokens: u32 = 280,
    /// How different a video frame must be from the last one kept to start a new shot: the mean difference of a
    /// 16 × 9 grey thumbnail, 0 to 255.
    shot_change: f64 = 14.0,
    /// Within one shot, a frame is kept at least this often, in seconds.
    keep_every_s: f64 = 30.0,
    /// A video's sound is read too: heard, and at `speech`, written down.
    sound: bool = true,
    /// Text is cut into passages of about this many characters (about 300 tokens of English). Shorter passages point
    /// closer to the words that matched; longer ones carry more context per vector.
    passage_chars: usize = 1200,
    /// A passage is cut at this many characters at the latest.
    max_passage_chars: usize = 2000,
}

/// How files are read. A file's values resolve in four steps, the last one that sets a value winning: the values at
/// the top of `reading`, its kind's (`reading.video`), its file type's (`reading.types.mov`), then each rule whose
/// path matches it, in order (`reading.rules`). A value config.json leaves out keeps its default.
#[derive(Debug, Clone, PartialEq)]
pub struct Reading {
    pub all: Values,
    pub kinds: BTreeMap<Kind, Values>,
    pub types: BTreeMap<String, Values>,
    pub rules: Vec<Rule>,
}

/// Reading values for the files under a path (see Pattern).
#[derive(Debug, Clone, PartialEq)]
pub struct Rule {
    pub path: Pattern,
    pub values: Values,
}

impl Default for Reading {
    fn default() -> Self {
        Self {
            all: Values::base(),
            kinds: BTreeMap::from([
                (Kind::Plaintext, Values::default()),
                (Kind::Document, Values::default()),
                // Records are read one by one (about 30 tokens): a passage of several blurs what each says. Short
                // ones, like a setting's line, go together.
                (Kind::Structured, Values { passage_chars: Some(120), ..Default::default() }),
                (Kind::Image, Values::default()),
                (Kind::Audio, Values::default()),
                (Kind::Video, Values { tokens: Some(70), ..Default::default() }),
                (Kind::Code, Values { passage_chars: Some(600), ..Default::default() }),
            ]),
            types: BTreeMap::new(),
            rules: Vec::new(),
        }
    }
}

impl Reading {
    /// What a file is read with.
    pub fn resolve(&self, path: &Path, kind: Kind) -> Settings {
        let mut settings = Settings::BASE;
        settings.apply(&self.all);
        if let Some(values) = self.kinds.get(&settings_kind(kind)) {
            settings.apply(values);
        }
        if let Some(values) = self.types.get(&extension_of(path)) {
            settings.apply(values);
        }
        for rule in &self.rules {
            if rule.path.matches(path) {
                settings.apply(&rule.values);
            }
        }
        settings
    }
}

/// How deep a file is read. Each depth holds the ones before it, and every file is read whole at its depth: what
/// was left to limits once (waiting, memory) the queue's order and streaming readers take care of.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Depth {
    /// Found by its name, its metadata and the words in it: text, a PDF's text layer, text on pictures. No model.
    Words,
    /// Also by what it means: its passages, pictures, video frames and sound, embedded.
    Meaning,
    /// Also by what's said in it, written down (in the background, once everything else is read) and embedded.
    Speech,
}

impl Settings {
    /// What's in the file is embedded.
    pub fn embeds(&self) -> bool {
        self.depth >= Depth::Meaning
    }
}

/// How `reading` is written in config.json: the values for everything, each kind's, then `types` and `rules`.
impl Serialize for Reading {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct File<'a> {
            #[serde(flatten)]
            all: &'a Values,
            #[serde(flatten)]
            kinds: &'a BTreeMap<Kind, Values>,
            types: &'a BTreeMap<String, Values>,
            rules: Vec<RuleFile<'a>>,
        }
        #[derive(Serialize)]
        struct RuleFile<'a> {
            path: &'a Pattern,
            #[serde(flatten)]
            values: &'a Values,
        }
        let rules = self.rules.iter().map(|rule| RuleFile { path: &rule.path, values: &rule.values }).collect();
        File { all: &self.all, kinds: &self.kinds, types: &self.types, rules }.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Reading {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let map = Map::<String, Value>::deserialize(deserializer)?;
        let values = |value: Value, at: &str| {
            serde_json::from_value::<Values>(value).map_err(|error| D::Error::custom(format!("reading{at}: {error}")))
        };
        let mut reading = Reading::default();
        let mut all = Map::new();
        for (key, value) in map {
            if key == "types" {
                let Value::Object(types) = value else { return Err(D::Error::custom("reading.types: not an object")) };
                for (file_type, value) in types {
                    let parsed = values(value, &format!(".types.{file_type}"))?;
                    reading.types.entry(file_type.trim_start_matches('.').to_lowercase()).or_default().overlay(parsed);
                }
            } else if key == "rules" {
                let Value::Array(rules) = value else { return Err(D::Error::custom("reading.rules: not a list")) };
                for (index, rule) in rules.into_iter().enumerate() {
                    let Value::Object(mut rule) = rule else {
                        return Err(D::Error::custom(format!("reading.rules[{index}]: not an object")));
                    };
                    let path = match rule.remove("path") {
                        Some(Value::String(path)) => Pattern::new(&path).map_err(D::Error::custom)?,
                        _ => return Err(D::Error::custom(format!("reading.rules[{index}]: which path?"))),
                    };
                    let parsed = values(Value::Object(rule), &format!(".rules[{index}]"))?;
                    reading.rules.push(Rule { path, values: parsed });
                }
            } else if let Some(kind) = Kind::parse(&key) {
                reading.kinds.entry(kind).or_default().overlay(values(value, &format!(".{key}"))?);
            } else {
                all.insert(key, value);
            }
        }
        reading.all.overlay(values(Value::Object(all), "")?);
        Ok(reading)
    }
}

/// The kind whose settings a kind takes: a screenshot takes the image kind's.
fn settings_kind(kind: Kind) -> Kind {
    if kind == Kind::Screenshot { Kind::Image } else { kind }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn values_resolve_in_order() {
        let mut config = Config::default();
        config.set("reading.video.keep_every_s", "10").unwrap();
        config.set("reading.types.mov", r#"{"keep_every_s": 20}"#).unwrap();
        config.set("reading.rules", r#"[{"path": "/talks", "keep_every_s": 5}]"#).unwrap();
        let reading = &config.reading;
        assert_eq!(reading.resolve(Path::new("/a/clip.mp4"), Kind::Video).keep_every_s, 10.0);
        assert_eq!(reading.resolve(Path::new("/a/clip.mov"), Kind::Video).keep_every_s, 20.0);
        assert_eq!(reading.resolve(Path::new("/talks/one.mov"), Kind::Video).keep_every_s, 5.0);
        assert_eq!(reading.resolve(Path::new("/a/clip.mp4"), Kind::Video).tokens, 70);
        assert_eq!(reading.resolve(Path::new("/a/song.mp3"), Kind::Audio).depth, Depth::Speech);
        config.set("reading.image.depth", "words").unwrap();
        assert_eq!(config.reading.resolve(Path::new("/a/shot.png"), Kind::Screenshot).depth, Depth::Words);
        assert!(config.set("reading.depth", "deep").is_err());
    }

    #[test]
    fn a_reading_value_changes_only_its_kinds_fingerprints() {
        let mut config = Config::default();
        let before = |config: &Config, kind| {
            let path = Path::new("/a/f");
            crate::ingest::fingerprint(path, kind, &config.reading.resolve(path, kind))
        };
        let (video, text) = (before(&config, Kind::Video), before(&config, Kind::Plaintext));
        config.set("reading.keep_every_s", "12").unwrap();
        assert_ne!(before(&config, Kind::Video), video);
        assert_eq!(before(&config, Kind::Plaintext), text);
        // Speech is written down after reading: no file is read again for it.
        config.set("reading.depth", "meaning").unwrap();
        assert_eq!(before(&config, Kind::Plaintext), text);
        // Without embedding, only what decides the words counts.
        config.set("reading.depth", "words").unwrap();
        let words = before(&config, Kind::Video);
        config.set("reading.keep_every_s", "20").unwrap();
        assert_eq!(before(&config, Kind::Video), words);
    }
}
