use codex_protocol::config_types::ApprovalsReviewer;
use codex_protocol::models::ActivePermissionProfile;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_rollout::HostReplayItem;
use codex_rollout::RolloutItem;
use codex_rollout::host_replay_items;

#[derive(Debug, PartialEq, Eq)]
pub(super) struct PersistedResumeSettings {
    pub(super) approval_policy: AskForApproval,
    pub(super) approvals_reviewer: Option<ApprovalsReviewer>,
    pub(super) active_permission_profile: Option<ActivePermissionProfile>,
}

pub(super) fn latest_persisted_resume_settings(
    history: &[RolloutItem],
) -> Option<PersistedResumeSettings> {
    let mut remaining = host_replay_items(history).rev();
    while let Some(item) = remaining.next() {
        match item {
            HostReplayItem::Item(RolloutItem::TurnContext(turn_context)) => {
                return Some(PersistedResumeSettings {
                    approval_policy: turn_context.approval_policy,
                    approvals_reviewer: turn_context.approvals_reviewer.or_else(|| {
                        remaining.clone().find_map(|item| match item {
                            HostReplayItem::Item(RolloutItem::TurnContext(turn_context)) => {
                                turn_context.approvals_reviewer
                            }
                            HostReplayItem::Item(RolloutItem::EventMsg(
                                EventMsg::ThreadSettingsApplied(event),
                            )) => Some(event.thread_settings.approvals_reviewer),
                            _ => None,
                        })
                    }),
                    active_permission_profile: turn_context.active_permission_profile.clone(),
                });
            }
            HostReplayItem::Item(RolloutItem::EventMsg(EventMsg::ThreadSettingsApplied(event))) => {
                return Some(PersistedResumeSettings {
                    approval_policy: event.thread_settings.approval_policy,
                    approvals_reviewer: Some(event.thread_settings.approvals_reviewer),
                    active_permission_profile: event
                        .thread_settings
                        .active_permission_profile
                        .clone(),
                });
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
#[path = "persisted_resume_settings_tests.rs"]
mod tests;
