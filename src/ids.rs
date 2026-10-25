//! Name validation and id generation.

use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use cuid2::CuidConstructor;
use ulid::Generator;

use crate::model::HUMAN;

/// Longest allowed agent name.
pub const MAX_NAME_LEN: usize = 32;

/// Length of the cuid2 suffix in an address.
pub const ADDRESS_SUFFIX_LEN: u16 = 8;

static ADDRESS_SUFFIX: CuidConstructor = CuidConstructor::new().with_length(ADDRESS_SUFFIX_LEN);
static ULIDS: Mutex<Generator> = Mutex::new(Generator::new());

/// Check an agent name against `^[a-z0-9][a-z0-9-]{0,31}$`. `human` is reserved.
pub fn validate_name(name: &str) -> Result<(), String> {
    if name == HUMAN {
        return Err(format!("name {HUMAN:?} is reserved"));
    }
    let bytes = name.as_bytes();
    let Some(first) = bytes.first() else {
        return Err("name must not be empty".to_string());
    };
    if bytes.len() > MAX_NAME_LEN {
        return Err(format!(
            "name {name:?} is longer than {MAX_NAME_LEN} characters"
        ));
    }
    let alnum = |b: &u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    if !alnum(first) {
        return Err(format!(
            "name {name:?} must start with a lowercase letter or digit"
        ));
    }
    if !bytes.iter().all(|b| alnum(b) || *b == b'-') {
        return Err(format!(
            "name {name:?} may only contain lowercase letters, digits and '-'"
        ));
    }
    Ok(())
}

/// `name` plus `-` plus an 8-character cuid2.
///
/// Uniqueness against existing addresses is the store's job: it re-rolls on
/// collision.
pub fn new_address(name: &str) -> String {
    format!("{name}-{}", ADDRESS_SUFFIX.create_id())
}

/// A new ULID string, strictly increasing within this process.
pub fn new_message_id() -> String {
    let mut generator = ULIDS.lock().unwrap_or_else(|e| e.into_inner());
    let ulid = match generator.generate() {
        Ok(ulid) => ulid,
        Err(overflow) => overflow.commit_overflow_increment(),
    };
    ulid.to_string()
}

/// Current time as Unix milliseconds.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn ids_valid_names_are_accepted() {
        for n in ["backend", "orders-api", "a", "0a", "a-", &"a".repeat(32)] {
            assert!(validate_name(n).is_ok(), "{n:?} should be valid");
        }
    }

    #[test]
    fn ids_invalid_names_are_rejected() {
        for n in [
            "",
            "-x",
            "Backend",
            "a_b",
            "a b",
            "é",
            &"a".repeat(33),
            "human",
        ] {
            assert!(validate_name(n).is_err(), "{n:?} should be invalid");
        }
    }

    #[test]
    fn ids_human_is_reserved() {
        let err = validate_name("human").unwrap_err();
        assert!(err.contains("reserved"), "{err}");
    }

    #[test]
    fn ids_new_address_has_name_and_8_char_cuid2() {
        let a = new_address("backend");
        let suffix = a.strip_prefix("backend-").expect("prefix");
        assert_eq!(suffix.len(), 8, "{a}");
        assert!(
            suffix
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()),
            "{a}"
        );
    }

    #[test]
    fn ids_new_address_is_unique() {
        let set: HashSet<String> = (0..1000).map(|_| new_address("backend")).collect();
        assert_eq!(set.len(), 1000);
    }

    #[test]
    fn ids_message_ids_increase() {
        let a = new_message_id();
        let b = new_message_id();
        assert!(b > a, "{b} should sort after {a}");
        assert_eq!(a.len(), 26);
    }

    #[test]
    fn ids_message_ids_increase_in_a_tight_loop() {
        let ids: Vec<String> = (0..1000).map(|_| new_message_id()).collect();
        assert!(ids.windows(2).all(|w| w[1] > w[0]));
    }

    #[test]
    fn ids_now_ms_is_plausible() {
        // After 2020-01-01 in milliseconds.
        assert!(now_ms() > 1_577_836_800_000);
    }
}
