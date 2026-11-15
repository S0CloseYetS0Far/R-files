//! Keeps a sleeping graphics card asleep.
//!
//! On laptops with two graphics cards (say Intel + NVIDIA), the second one
//! is powered down while nothing uses it. GTK's Vulkan start-up asks every
//! installed Vulkan driver for its devices, which wakes that card: about
//! 1.4 s of waiting on every launch, plus every driver's libraries in
//! memory, for a card RFiles never draws with. So when a card is asleep and
//! another vendor's card is awake, Vulkan is told to use only the awake
//! card's driver.

use std::path::Path;
use std::sync::OnceLock;

const NVIDIA: u32 = 0x10de;
const AMD: u32 = 0x1002;
const INTEL: u32 = 0x8086;

/// One graphics card: PCI vendor id and whether it is powered down.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Card {
    pub vendor: u32,
    pub asleep: bool,
}

/// Vulkan driver names (as globs for the Vulkan loader) for a vendor.
fn driver_globs(vendor: u32) -> Option<&'static [&'static str]> {
    match vendor {
        NVIDIA => Some(&["*nvidia*", "*nouveau*"]),
        AMD => Some(&["*radeon*"]),
        INTEL => Some(&["*intel*"]),
        _ => None,
    }
}

/// Value for `VK_LOADER_DRIVERS_SELECT`, or `None` to leave Vulkan alone.
/// When a card is asleep and a card from another vendor is awake, only the
/// awake cards' drivers are used. Anything unusual (one card, same vendor
/// twice, a vendor we don't know) changes nothing.
pub fn drivers_to_use(cards: &[Card]) -> Option<String> {
    let awake: Vec<u32> = cards.iter().filter(|c| !c.asleep).map(|c| c.vendor).collect();
    let sleeper = cards.iter().any(|c| c.asleep && !awake.contains(&c.vendor));
    if !sleeper || awake.is_empty() {
        return None;
    }
    let mut globs: Vec<&str> = Vec::new();
    for v in awake {
        for g in driver_globs(v)? {
            if !globs.contains(g) {
                globs.push(g);
            }
        }
    }
    Some(globs.join(","))
}

fn cards() -> Vec<Card> {
    let mut out = Vec::new();
    let Ok(dir) = std::fs::read_dir("/sys/class/drm") else { return out };
    for e in dir.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        // "card0", "card1"…, not the outputs ("card1-eDP-1")
        let Some(n) = name.strip_prefix("card") else { continue };
        if n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let dev = e.path().join("device");
        let Some(vendor) = read(&dev.join("vendor"))
            .and_then(|v| u32::from_str_radix(v.trim_start_matches("0x"), 16).ok())
        else {
            continue;
        };
        let asleep = read(&dev.join("power/runtime_status")).as_deref() == Some("suspended");
        out.push(Card { vendor, asleep });
    }
    out
}

fn read(p: &Path) -> Option<String> {
    std::fs::read_to_string(p).ok().map(|s| s.trim().to_string())
}

const SELECT: &str = "VK_LOADER_DRIVERS_SELECT";

/// The variable this process set for itself, so programs it starts can be
/// given the environment without it (they may want the sleeping card).
static SET_HERE: OnceLock<&'static str> = OnceLock::new();

/// What to set: `(variable, value)`, or `None` when the user already chose
/// Vulkan drivers or a renderer themselves (`is_set` says whether a
/// variable is in the environment) or nothing needs choosing.
/// `cards` is only asked for when needed (it reads /sys).
fn choice(is_set: impl Fn(&str) -> bool, cards: impl FnOnce() -> Vec<Card>) -> Option<(&'static str, String)> {
    const OWN_CHOICE: [&str; 5] =
        ["VK_LOADER_DRIVERS_DISABLE", SELECT, "VK_DRIVER_FILES", "VK_ICD_FILENAMES", "GSK_RENDERER"];
    if OWN_CHOICE.iter().any(|v| is_set(v)) {
        return None;
    }
    drivers_to_use(&cards()).map(|only| (SELECT, only))
}

/// Must run first thing in `main`, before any thread exists (it changes
/// the environment) and before GTK loads Vulkan. It stays set: GTK may
/// create renderers again for later windows, and removing it once threads
/// run isn't safe. Programs RFiles starts drop it, see `set_here`.
pub fn keep_sleeping_cards_asleep() {
    if let Some((var, only)) = choice(|v| std::env::var_os(v).is_some(), cards) {
        // SAFETY: called at the top of `main`, while this is the only thread.
        unsafe { std::env::set_var(var, only) };
        remember(var);
    }
}

pub(crate) fn remember(var: &'static str) {
    let _ = SET_HERE.set(var);
}

/// The environment variable RFiles set for itself (not one the user set),
/// to be removed from the environment of every program it starts.
pub fn set_here() -> Option<&'static str> {
    SET_HERE.get().copied()
}

#[cfg(test)]
mod choice_tests {
    use super::*;

    fn laptop() -> Vec<Card> {
        vec![Card { vendor: INTEL, asleep: false }, Card { vendor: NVIDIA, asleep: true }]
    }

    #[test]
    fn sets_select_on_a_laptop_with_a_sleeping_card() {
        assert_eq!(choice(|_| false, laptop), Some((SELECT, "*intel*".to_string())));
        assert_eq!(choice(|_| false, || laptop()[..1].to_vec()), None);
    }

    #[test]
    fn leaves_the_users_own_choice_alone() {
        // Their value is theirs: not overwritten, and not remembered (so children keep it)
        for own in [SELECT, "VK_ICD_FILENAMES", "GSK_RENDERER"] {
            assert_eq!(choice(|v| v == own, || panic!("no need to look at the cards")), None, "{own}");
        }
        // Unrelated variables don't count
        assert!(choice(|v| v == "VK_INSTANCE_LAYERS", laptop).is_some());
    }

    #[test]
    fn nothing_remembered_unless_set() {
        // Tests never call keep_sleeping_cards_asleep, so this process set nothing
        assert_eq!(set_here(), None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(vendor: u32, asleep: bool) -> Card {
        Card { vendor, asleep }
    }

    #[test]
    fn uses_only_awake_cards_when_another_sleeps() {
        // Laptop: Intel draws, NVIDIA sleeps
        assert_eq!(drivers_to_use(&[c(INTEL, false), c(NVIDIA, true)]).as_deref(), Some("*intel*"));
        assert_eq!(drivers_to_use(&[c(NVIDIA, true), c(AMD, false)]).as_deref(), Some("*radeon*"));
        // Both awake (a game is running): leave everything alone
        assert_eq!(drivers_to_use(&[c(INTEL, false), c(NVIDIA, false)]), None);
        // One card only, even if it reports asleep: never cut it off
        assert_eq!(drivers_to_use(&[c(NVIDIA, true)]), None);
        // Two cards of one vendor share a driver: nothing to choose
        assert_eq!(drivers_to_use(&[c(AMD, false), c(AMD, true)]), None);
        // An awake card we don't know (a virtual machine's): hands off
        assert_eq!(drivers_to_use(&[c(0x1af4, false), c(NVIDIA, true)]), None);
        assert_eq!(drivers_to_use(&[]), None);
    }
}
