//! Small native control-tool effects share the existing Panng history owner.
use super::*;
use codex_panng_runtime::RuntimeMutation;

impl Session {
    pub(crate) async fn commit_panng_control_effect(
        &self,
        turn: &TurnContext,
        model: &ModelInfo,
        effect: RolloutItem,
    ) -> CodexResult<()> {
        self.begin_panng_history(turn, model, Vec::new())
            .await?
            .commit_with_effects(
                RuntimeMutation::delta(Vec::new())
                    .map_err(|error| CodexErr::Fatal(error.to_string()))?,
                vec![effect],
            )
            .await?;
        Ok(())
    }
}
