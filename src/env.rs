use std::collections::BTreeMap;

pub trait EnvReader {
    fn get(&self, key: &str) -> Option<String>;
}

#[derive(Debug, Default)]
pub struct OsEnv;

impl EnvReader for OsEnv {
    fn get(&self, key: &str) -> Option<String> {
        std::env::var(key)
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
    }
}

impl EnvReader for BTreeMap<String, String> {
    fn get(&self, key: &str) -> Option<String> {
        self.get(key)
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
    }
}
