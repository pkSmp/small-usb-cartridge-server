//! cartridge config.json: one `sounds` object naming every dialogue the user
//! wants to hear, with the file inside the stick's `sounds/` folder.
//!
//!   {
//!     "sounds": {
//!       "BOOT": "the_computer_started_correctly.wav",
//!       "DIGIT-7": "seven.wav",
//!       "UNIT-GB-RAM": "gigabytes_of_ram.wav"
//!     }
//!   }
//!
//! Every key is optional: delete a line and that dialogue is skipped. Set a
//! value to "" to silence one without deleting the key. Unknown keys are kept,
//! so a cartridge can name extra lines and play them with `cartridge say`.

use crate::catalog::LINES;
use crate::json::{self, Value};
use std::collections::BTreeMap;
use std::path::Path;

pub struct Config {
    pub sounds: BTreeMap<String, String>,
}

impl Default for Config {
    fn default() -> Config {
        let mut sounds = BTreeMap::new();
        for (key, text) in LINES {
            sounds.insert(key.to_string(), crate::voice::default_file_name(text));
        }
        Config { sounds }
    }
}

pub fn load(path: &Path) -> Result<Config, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {}", path.display(), e))?;
    let value = json::parse(&text).map_err(|e| format!("{}: {}", path.display(), e))?;
    match value {
        Value::Obj(_) => {}
        _ => return Err("config.json must contain a JSON object".into()),
    }
    let mut config = Config::default();
    if let Some(Value::Obj(items)) = value.get("sounds") {
        for (key, val) in items {
            match val.as_str() {
                Some("") => {
                    config.sounds.remove(key);
                }
                Some(file) => {
                    config.sounds.insert(key.clone(), file.trim().to_string());
                }
                None => {}
            }
        }
    }
    Ok(config)
}
