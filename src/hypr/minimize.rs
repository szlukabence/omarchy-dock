//! Minimize, for a compositor that has none.
//!
//! A minimized window is one parked on the `special:minimized` workspace:
//! being there is what "minimized" means, so the dock keeps no list of its
//! own. Before it goes, the window is tagged with the workspace it came from
//! (`omarchy-dock-home:3`). Hyprland holds that tag, so a dock that restarts
//! still knows where every minimized window belongs, and nothing is written
//! to disk.

/// Where minimized windows are parked.
pub const WORKSPACE: &str = "special:minimized";

/// Tag prefix recording a minimized window's home workspace.
pub const HOME_TAG: &str = "omarchy-dock-home:";

/// One dispatcher call against the window being minimized or restored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// `+tag` adds a tag, `-tag` removes it.
    Tag(String),
    Move { workspace: String, follow: bool },
    Focus,
}

/// Whether `home` names somewhere a window can go back to.
pub fn is_home(home: &str) -> bool {
    !home.is_empty() && home != WORKSPACE
}

/// The home workspace recorded in a window's tags, if any.
pub fn home_of(tags: &[String]) -> Option<String> {
    tags.iter()
        .find_map(|t| t.strip_prefix(HOME_TAG))
        .filter(|h| is_home(h))
        .map(str::to_string)
}

/// How a dispatcher names workspace `name`: numbers and special workspaces
/// as they are, anything else by name. A bare word would be read as a new
/// workspace of that number or name.
fn selector(name: &str) -> String {
    if name.parse::<i64>().is_ok() || name.starts_with("special:") {
        name.to_string()
    } else {
        format!("name:{name}")
    }
}

/// Where a restored window goes: home, else the workspace in front.
pub fn restore_target(home: Option<&str>, current: &str) -> String {
    selector(home.filter(|h| is_home(h)).unwrap_or(current))
}

/// Tag first, then move: the move is what makes the dock resync, and the
/// snapshot it takes must already see where the window came from.
pub fn minimize_steps(workspace: &str, stale_home: Option<&str>) -> Vec<Step> {
    let mut steps = Vec::new();
    if let Some(old) = stale_home {
        steps.push(Step::Tag(format!("-{HOME_TAG}{old}")));
    }
    steps.push(Step::Tag(format!("+{HOME_TAG}{workspace}")));
    steps.push(Step::Move { workspace: WORKSPACE.into(), follow: false });
    steps
}

/// Send the window home and follow it there, untag it, then focus it. The
/// tag goes only once the move has worked: a window left parked by a failed
/// move still knows where home is.
pub fn restore_steps(home: Option<&str>, current: &str) -> Vec<Step> {
    let mut steps =
        vec![Step::Move { workspace: restore_target(home, current), follow: true }];
    if let Some(h) = home {
        steps.push(Step::Tag(format!("-{HOME_TAG}{h}")));
    }
    steps.push(Step::Focus);
    steps
}

/// A minimized window dropped on a workspace tile: send it there without
/// following — a drop puts a window away, it doesn't go to it — then untag it.
pub fn unpark_steps(home: Option<&str>, workspace: &str) -> Vec<Step> {
    let mut steps = vec![Step::Move { workspace: workspace.into(), follow: false }];
    if let Some(h) = home {
        steps.push(Step::Tag(format!("-{HOME_TAG}{h}")));
    }
    steps
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tags(t: &[&str]) -> Vec<String> {
        t.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_home_workspace_is_read_from_the_tag() {
        assert_eq!(home_of(&tags(&["default-opacity*", "omarchy-dock-home:3"])), Some("3".into()));
        assert_eq!(home_of(&tags(&["omarchy-dock-home:special:scratchpad"])), Some("special:scratchpad".into()));
        assert_eq!(home_of(&tags(&["default-opacity*"])), None);
        // An empty home, or one naming the parking workspace itself, is no home.
        assert_eq!(home_of(&tags(&["omarchy-dock-home:"])), None);
        assert_eq!(home_of(&tags(&["omarchy-dock-home:special:minimized"])), None);
    }

    #[test]
    fn a_restored_window_is_sent_home_by_a_name_hyprland_accepts() {
        assert_eq!(restore_target(Some("3"), "1"), "3");
        assert_eq!(restore_target(Some("special:scratchpad"), "1"), "special:scratchpad");
        // A named workspace needs `name:`, or Hyprland reads it as a new one.
        assert_eq!(restore_target(Some("mail"), "1"), "name:mail");
        // No home: the workspace in front, by the same rule.
        assert_eq!(restore_target(None, "2"), "2");
        assert_eq!(restore_target(None, "web"), "name:web");
        assert_eq!(restore_target(Some("special:minimized"), "4"), "4");
    }

    #[test]
    fn minimizing_tags_the_window_before_moving_it() {
        // The move triggers the resync; the tag must already be there.
        assert_eq!(
            minimize_steps("3", None),
            vec![
                Step::Tag("+omarchy-dock-home:3".into()),
                Step::Move { workspace: "special:minimized".into(), follow: false },
            ]
        );
        // A stale home from an earlier trip is dropped first.
        assert_eq!(
            minimize_steps("3", Some("5")),
            vec![
                Step::Tag("-omarchy-dock-home:5".into()),
                Step::Tag("+omarchy-dock-home:3".into()),
                Step::Move { workspace: "special:minimized".into(), follow: false },
            ]
        );
    }

    #[test]
    fn restoring_follows_the_window_home_then_untags_it() {
        // Moved first: if the move fails, the window is still parked and
        // still knows where home is.
        assert_eq!(
            restore_steps(Some("3"), "1"),
            vec![
                Step::Move { workspace: "3".into(), follow: true },
                Step::Tag("-omarchy-dock-home:3".into()),
                Step::Focus,
            ]
        );
        assert_eq!(
            restore_steps(None, "2"),
            vec![Step::Move { workspace: "2".into(), follow: true }, Step::Focus]
        );
    }

    #[test]
    fn a_minimized_window_dropped_on_a_workspace_goes_there_untagged() {
        // Sent, not followed: a drop puts a window away somewhere.
        assert_eq!(
            unpark_steps(Some("3"), "5"),
            vec![
                Step::Move { workspace: "5".into(), follow: false },
                Step::Tag("-omarchy-dock-home:3".into()),
            ]
        );
        assert_eq!(
            unpark_steps(None, "special:scratchpad"),
            vec![Step::Move { workspace: "special:scratchpad".into(), follow: false }]
        );
    }

    #[test]
    fn clients_parse_with_and_without_tags() {
        let base = r#""address":"0x1","class":"c","title":"t","initialClass":"c",
            "workspace":{"id":-98,"name":"special:minimized"},"monitor":0,"pid":1,
            "floating":false,"hidden":false,"mapped":true,"fullscreen":0,
            "at":[0,0],"size":[1,1]"#;
        let old: crate::hypr::model::Client = serde_json::from_str(&format!("{{{base}}}")).unwrap();
        assert!(old.tags.is_empty());
        assert!(old.is_minimized());
        let new: crate::hypr::model::Client =
            serde_json::from_str(&format!(r#"{{{base},"tags":["omarchy-dock-home:2"]}}"#)).unwrap();
        assert_eq!(home_of(&new.tags), Some("2".into()));
    }

    #[test]
    fn a_grouped_window_knows_its_tab_mates_but_not_itself() {
        let json = r#"{"address":"0x1","class":"c","title":"t","initialClass":"c",
            "workspace":{"id":3,"name":"3"},"monitor":0,"pid":1,
            "floating":false,"hidden":false,"mapped":true,"fullscreen":0,
            "at":[0,0],"size":[1,1],"grouped":["0x1","0x2"]}"#;
        let c: crate::hypr::model::Client = serde_json::from_str(json).unwrap();
        assert_eq!(c.tab_mates(), vec![crate::hypr::Address::parse("0x2")]);
    }
}
