use super::*;
use crate::legacy_core::config::ConfigBuilder;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn panng_config_opt_in_selects_history_without_changing_native_defaults() -> Result<()> {
    let home = tempfile::tempdir()?;
    let config = ConfigBuilder::default()
        .codex_home(home.path().to_path_buf())
        .fallback_cwd(Some(home.path().to_path_buf()))
        .cli_overrides(vec![("experimental_panng".into(), true.into())])
        .build()
        .await?;
    for mode in [ThreadParamsMode::Embedded, ThreadParamsMode::Remote] {
        let mut native = config.clone();
        native.experimental_panng = false;
        let mut expected = thread_start_params_from_config(
            &native, mode, /*remote_cwd_override*/ None, /*session_start_source*/ None,
        );
        assert_eq!(expected.history_mode, Some(ThreadHistoryMode::Paginated));
        expected.history_mode = Some(ThreadHistoryMode::PanngV1);
        assert_eq!(
            thread_start_params_from_config(
                &config, mode, /*remote_cwd_override*/ None,
                /*session_start_source*/ None,
            ),
            expected,
        );
    }
    Ok(())
}
