//! Turning what openvpn3 stores into something worth putting in a menu.

/// A menu-ready label for a profile.
///
/// Two things make the raw name unsuitable on its own.
///
/// `openvpn3 config-import` names a profile after the file it came from unless
/// `--name` is passed, and that is usually an absolute path — so a profile can
/// legitimately be called `/home/you/Downloads/work.ovpn`. Showing the stem is
/// what the user meant by the name.
///
/// And when the name cannot be read at all, the fallback must not be the D-Bus
/// object path: `/net/openvpn/v3/configuration/a1b2c3` in a menu is noise, not
/// information.
pub fn display_name(raw_name: &str, config_path: &str) -> String {
    let trimmed = raw_name.trim();

    if trimmed.is_empty() {
        return fallback_label(config_path);
    }

    // Path-shaped names come from a CLI import; show what the user would call it.
    if trimmed.contains('/') {
        let path = std::path::Path::new(trimmed);
        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
            if !stem.is_empty() {
                return stem.to_owned();
            }
        }
    }

    trimmed.to_owned()
}

/// Last segment of the object path, so an unreadable profile is still
/// distinguishable from its neighbours without dumping the whole path.
fn fallback_label(config_path: &str) -> String {
    let id = config_path.rsplit('/').next().unwrap_or(config_path);
    if id.is_empty() {
        "Unnamed profile".to_owned()
    } else {
        format!("Profile {id}")
    }
}

#[cfg(test)]
mod tests {
    use super::display_name;

    const PATH: &str = "/net/openvpn/v3/configuration/a1b2c3";

    #[test]
    fn a_plain_name_is_used_as_is() {
        assert_eq!(display_name("myvpn", PATH), "myvpn");
        assert_eq!(display_name("dale-stg", PATH), "dale-stg");
    }

    #[test]
    fn surrounding_whitespace_is_trimmed() {
        assert_eq!(display_name("  myvpn \n", PATH), "myvpn");
    }

    /// The common case after `openvpn3 config-import --config ~/x/work.ovpn`.
    #[test]
    fn a_path_shaped_name_shows_its_stem() {
        assert_eq!(
            display_name("/home/alonso/Downloads/work.ovpn", PATH),
            "work"
        );
        assert_eq!(display_name("./configs/dale-stg.ovpn", PATH), "dale-stg");
    }

    #[test]
    fn a_path_without_an_extension_still_resolves() {
        assert_eq!(display_name("/opt/vpn/corporate", PATH), "corporate");
    }

    /// A name containing a slash but ending in one has no stem to take.
    #[test]
    fn a_trailing_slash_falls_back_rather_than_producing_nothing() {
        let label = display_name("/home/alonso/", PATH);
        assert!(!label.is_empty());
        assert_eq!(label, "alonso");
    }

    #[test]
    fn an_empty_name_falls_back_to_the_object_path_id_not_the_whole_path() {
        let label = display_name("", PATH);
        assert_eq!(label, "Profile a1b2c3");
        assert!(
            !label.contains("/net/openvpn"),
            "the D-Bus path is noise in a menu, not information"
        );
    }

    #[test]
    fn whitespace_only_names_are_treated_as_empty() {
        assert_eq!(display_name("   ", PATH), "Profile a1b2c3");
    }

    /// Two unreadable profiles must not collapse into the same label, or the
    /// menu offers two identical rows that do different things.
    #[test]
    fn fallback_labels_stay_distinct_between_profiles() {
        let a = display_name("", "/net/openvpn/v3/configuration/aaa");
        let b = display_name("", "/net/openvpn/v3/configuration/bbb");
        assert_ne!(a, b);
    }
}
