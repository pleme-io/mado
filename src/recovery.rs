use std::fmt;

use engate_types::AttachError;
use serde::{Deserialize, Serialize};
use tear_types::{ControlError, MultiplexerControl, PaneId, SessionId};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FaultSite {
    pub pane: PaneId,
    pub session: Option<SessionId>,
    pub previous: Option<PaneId>,
}

impl FaultSite {
    #[must_use]
    pub fn new(pane: PaneId) -> Self {
        Self {
            pane,
            session: None,
            previous: None,
        }
    }

    #[must_use]
    pub fn with_previous(mut self, previous: Option<PaneId>) -> Self {
        self.previous = previous.filter(|p| *p != self.pane);
        self
    }

    #[must_use]
    pub fn located(mut self, control: &dyn MultiplexerControl) -> Self {
        self.session = control.list_sessions().ok().and_then(|sessions| {
            sessions
                .iter()
                .find(|s| s.panes.contains_key(&self.pane))
                .map(|s| s.id)
        });
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PaneFault {
    SnapshotTooLarge { site: FaultSite, detail: String },
    AttachFailed { site: FaultSite, detail: String },
    DaemonUnreachable { site: FaultSite, detail: String },
    PaneEnded { site: FaultSite },
    SessionGone { site: FaultSite },
    HistoryElided { site: FaultSite, rows: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(test, derive(pleme_allvariants_derive::AllVariants))]
pub enum PaneFaultKind {
    SnapshotTooLarge,
    AttachFailed,
    DaemonUnreachable,
    PaneEnded,
    SessionGone,
    HistoryElided,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(test, derive(pleme_allvariants_derive::AllVariants))]
pub enum RecoveryAction {
    Retry,
    SwitchToPrevious,
    OpenSessionPicker,
    KillSession,
    Dismiss,
}

impl fmt::Display for RecoveryAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Retry => "Retry",
            Self::SwitchToPrevious => "Return to the previous pane",
            Self::OpenSessionPicker => "Open the session picker",
            Self::KillSession => "Kill this session",
            Self::Dismiss => "Dismiss",
        })
    }
}

impl PaneFault {
    #[must_use]
    pub fn site(&self) -> &FaultSite {
        match self {
            Self::SnapshotTooLarge { site, .. }
            | Self::AttachFailed { site, .. }
            | Self::DaemonUnreachable { site, .. }
            | Self::PaneEnded { site }
            | Self::SessionGone { site }
            | Self::HistoryElided { site, .. } => site,
        }
    }

    #[must_use]
    pub fn kind(&self) -> PaneFaultKind {
        match self {
            Self::SnapshotTooLarge { .. } => PaneFaultKind::SnapshotTooLarge,
            Self::AttachFailed { .. } => PaneFaultKind::AttachFailed,
            Self::DaemonUnreachable { .. } => PaneFaultKind::DaemonUnreachable,
            Self::PaneEnded { .. } => PaneFaultKind::PaneEnded,
            Self::SessionGone { .. } => PaneFaultKind::SessionGone,
            Self::HistoryElided { .. } => PaneFaultKind::HistoryElided,
        }
    }

    #[must_use]
    pub fn blocking(&self) -> bool {
        self.kind().blocking()
    }

    #[must_use]
    pub fn same_as(&self, other: &Self) -> bool {
        self.kind() == other.kind() && self.site() == other.site()
    }

    #[must_use]
    pub fn from_control(site: FaultSite, error: &ControlError) -> Self {
        match error {
            e if e.is_response_too_large() => Self::SnapshotTooLarge {
                site,
                detail: e.to_string(),
            },
            ControlError::Transport(_) => Self::DaemonUnreachable {
                site,
                detail: error.to_string(),
            },
            ControlError::NoSuchPane(_) | ControlError::NoSuchWindow(_) => Self::PaneEnded { site },
            ControlError::NoSuchSession(_) => Self::SessionGone { site },
            ControlError::PartialInput { cause, .. } => Self::from_control(site, cause),
            ControlError::Rejected(_)
            | ControlError::Unsupported { .. }
            | ControlError::Internal(_) => Self::AttachFailed {
                site,
                detail: error.to_string(),
            },
        }
    }

    #[must_use]
    pub fn from_attach(site: FaultSite, error: &AttachError) -> Self {
        match error {
            AttachError::SnapshotFailed(_) | AttachError::SubscribeFailed(_) => {
                Self::AttachFailed {
                    site,
                    detail: error.to_string(),
                }
            }
            AttachError::NoSuchEntity(_) => Self::PaneEnded { site },
            AttachError::Transport(_) => Self::DaemonUnreachable {
                site,
                detail: error.to_string(),
            },
        }
    }
}

impl PaneFaultKind {
    #[must_use]
    pub fn blocking(self) -> bool {
        match self {
            Self::SnapshotTooLarge
            | Self::AttachFailed
            | Self::DaemonUnreachable
            | Self::PaneEnded
            | Self::SessionGone => true,
            Self::HistoryElided => false,
        }
    }
}

impl fmt::Display for PaneFault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SnapshotTooLarge { site, detail } => write!(
                f,
                "pane {} is too large for tear to send, so mado cannot show it ({detail})",
                site.pane
            ),
            Self::AttachFailed { site, detail } => {
                write!(f, "mado could not attach pane {}: {detail}", site.pane)
            }
            Self::DaemonUnreachable { site, detail } => write!(
                f,
                "the tear daemon does not answer for pane {}: {detail}",
                site.pane
            ),
            Self::PaneEnded { site } => write!(f, "pane {} has ended", site.pane),
            Self::SessionGone { site } => {
                write!(f, "the session of pane {} no longer exists", site.pane)
            }
            Self::HistoryElided { site, rows } => write!(
                f,
                "pane {}: {rows} oldest history rows were too large to load",
                site.pane
            ),
        }
    }
}

#[must_use]
pub fn actions_for(fault: &PaneFault) -> Vec<RecoveryAction> {
    use RecoveryAction::{Dismiss, KillSession, OpenSessionPicker, Retry, SwitchToPrevious};
    let site = fault.site();
    let previous = site.previous.map(|_| SwitchToPrevious);
    let kill = site.session.map(|_| KillSession);
    let rows: Vec<Option<RecoveryAction>> = match fault.kind() {
        PaneFaultKind::SnapshotTooLarge | PaneFaultKind::AttachFailed => vec![
            Some(Retry),
            previous,
            Some(OpenSessionPicker),
            kill,
            Some(Dismiss),
        ],
        PaneFaultKind::DaemonUnreachable => {
            vec![
                Some(Retry),
                previous,
                Some(OpenSessionPicker),
                Some(Dismiss),
            ]
        }
        PaneFaultKind::PaneEnded => vec![previous, Some(OpenSessionPicker), kill, Some(Dismiss)],
        PaneFaultKind::SessionGone => vec![previous, Some(OpenSessionPicker), Some(Dismiss)],
        PaneFaultKind::HistoryElided => vec![Some(Dismiss)],
    };
    rows.into_iter().flatten().collect()
}

pub type RecoveryMenuState = tear_picker::state::FuzzyPicker<RecoveryAction>;

#[cfg(test)]
mod tests {
    use super::*;

    fn full_site() -> FaultSite {
        FaultSite {
            pane: PaneId::from_seed("faulted"),
            session: Some(SessionId::from_seed("owner")),
            previous: Some(PaneId::from_seed("before")),
        }
    }

    fn sample(kind: PaneFaultKind, site: FaultSite) -> PaneFault {
        let detail = String::from("detail");
        match kind {
            PaneFaultKind::SnapshotTooLarge => PaneFault::SnapshotTooLarge { site, detail },
            PaneFaultKind::AttachFailed => PaneFault::AttachFailed { site, detail },
            PaneFaultKind::DaemonUnreachable => PaneFault::DaemonUnreachable { site, detail },
            PaneFaultKind::PaneEnded => PaneFault::PaneEnded { site },
            PaneFaultKind::SessionGone => PaneFault::SessionGone { site },
            PaneFaultKind::HistoryElided => PaneFault::HistoryElided { site, rows: 7 },
        }
    }

    #[test]
    fn every_fault_kind_has_a_row_and_every_action_is_offered_somewhere() {
        let mut offered = std::collections::BTreeSet::new();
        for kind in PaneFaultKind::ALL {
            let fault = sample(*kind, full_site());
            assert_eq!(fault.kind(), *kind);
            let actions = actions_for(&fault);
            assert!(!actions.is_empty(), "{kind:?} offers nothing");
            assert_eq!(
                actions.last(),
                Some(&RecoveryAction::Dismiss),
                "{kind:?}: Dismiss is always the last way out"
            );
            let unique: std::collections::BTreeSet<_> = actions.iter().map(|a| *a as u8).collect();
            assert_eq!(unique.len(), actions.len(), "{kind:?} repeats an action");
            assert!(
                !fault.to_string().contains('\n'),
                "{kind:?} Display is one line"
            );
            offered.extend(actions.iter().map(|a| *a as u8));
        }
        for action in RecoveryAction::ALL {
            assert!(
                offered.contains(&(*action as u8)),
                "{action:?} is offered for no fault"
            );
            assert!(!action.to_string().is_empty());
        }
    }

    #[test]
    fn only_the_history_notice_is_non_blocking() {
        for kind in PaneFaultKind::ALL {
            assert_eq!(
                kind.blocking(),
                *kind != PaneFaultKind::HistoryElided,
                "{kind:?}"
            );
        }
    }

    #[test]
    fn actions_follow_what_the_site_knows() {
        let bare = FaultSite::new(PaneId::from_seed("p"));
        let fault = sample(PaneFaultKind::AttachFailed, bare.clone());
        assert_eq!(
            actions_for(&fault),
            vec![
                RecoveryAction::Retry,
                RecoveryAction::OpenSessionPicker,
                RecoveryAction::Dismiss
            ]
        );
        let fault = sample(PaneFaultKind::SnapshotTooLarge, full_site());
        assert_eq!(
            actions_for(&fault),
            vec![
                RecoveryAction::Retry,
                RecoveryAction::SwitchToPrevious,
                RecoveryAction::OpenSessionPicker,
                RecoveryAction::KillSession,
                RecoveryAction::Dismiss
            ]
        );
        assert_eq!(
            actions_for(&sample(PaneFaultKind::HistoryElided, full_site())),
            vec![RecoveryAction::Dismiss]
        );
        assert!(
            !actions_for(&sample(PaneFaultKind::DaemonUnreachable, full_site()))
                .contains(&RecoveryAction::KillSession),
            "a daemon that does not answer cannot kill a session"
        );
        assert!(
            !actions_for(&sample(PaneFaultKind::PaneEnded, full_site()))
                .contains(&RecoveryAction::Retry)
        );
    }

    #[test]
    fn a_previous_pane_equal_to_the_faulted_pane_is_not_a_way_back() {
        let pane = PaneId::from_seed("same");
        assert_eq!(
            FaultSite::new(pane).with_previous(Some(pane)).previous,
            None
        );
        let other = PaneId::from_seed("other");
        assert_eq!(
            FaultSite::new(pane).with_previous(Some(other)).previous,
            Some(other)
        );
    }

    #[test]
    fn control_errors_classify_by_their_typed_shape() {
        let site = || FaultSite::new(PaneId::from_seed("p"));
        let too_large = ControlError::Rejected(format!(
            "{}: pane snapshot is 18 MB",
            tear_types::wire::RESPONSE_TOO_LARGE
        ));
        assert_eq!(
            PaneFault::from_control(site(), &too_large).kind(),
            PaneFaultKind::SnapshotTooLarge
        );
        assert_eq!(
            PaneFault::from_control(site(), &ControlError::Transport("gone".into())).kind(),
            PaneFaultKind::DaemonUnreachable
        );
        assert_eq!(
            PaneFault::from_control(site(), &ControlError::NoSuchPane(PaneId::from_seed("p")))
                .kind(),
            PaneFaultKind::PaneEnded
        );
        assert_eq!(
            PaneFault::from_control(
                site(),
                &ControlError::NoSuchSession(SessionId::from_seed("s"))
            )
            .kind(),
            PaneFaultKind::SessionGone
        );
        assert_eq!(
            PaneFault::from_control(site(), &ControlError::Rejected("other".into())).kind(),
            PaneFaultKind::AttachFailed
        );
        let partial = ControlError::PartialInput {
            delivered: 0,
            total: 4,
            cause: Box::new(ControlError::Transport("gone".into())),
        };
        assert_eq!(
            PaneFault::from_control(site(), &partial).kind(),
            PaneFaultKind::DaemonUnreachable
        );
    }

    #[test]
    fn attach_errors_classify_by_variant() {
        let site = || FaultSite::new(PaneId::from_seed("p"));
        for (error, want) in [
            (
                AttachError::SnapshotFailed("x".into()),
                PaneFaultKind::AttachFailed,
            ),
            (
                AttachError::SubscribeFailed("x".into()),
                PaneFaultKind::AttachFailed,
            ),
            (
                AttachError::NoSuchEntity("x".into()),
                PaneFaultKind::PaneEnded,
            ),
            (
                AttachError::Transport("x".into()),
                PaneFaultKind::DaemonUnreachable,
            ),
        ] {
            assert_eq!(PaneFault::from_attach(site(), &error).kind(), want);
        }
    }

    #[test]
    fn a_fault_round_trips_as_inspectable_data() {
        let fault = sample(PaneFaultKind::SnapshotTooLarge, full_site());
        let json = serde_json::to_value(&fault).expect("serialize");
        assert_eq!(json["kind"], "snapshot_too_large");
        let back: PaneFault = serde_json::from_value(json).expect("deserialize");
        assert_eq!(back, fault);
    }
}
