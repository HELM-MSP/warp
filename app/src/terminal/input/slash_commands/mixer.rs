use warpui::{ModelContext, ModelHandle};

use crate::search::data_source::{Query, QueryFilter};
use crate::search::mixer::{AddAsyncSourceOptions, SearchMixer};
use crate::terminal::input::slash_commands::{
    AcceptSlashCommandOrSavedPrompt, SlashCommandDataSource, ZeroStateDataSource,
};

pub type SlashCommandMixer = SearchMixer<AcceptSlashCommandOrSavedPrompt>;

pub fn build_slash_command_mixer(
    slash_commands_source: ModelHandle<SlashCommandDataSource>,
    is_cloud_mode_v2: bool,
    ctx: &mut ModelContext<SlashCommandMixer>,
) -> SlashCommandMixer {
    let mut mixer = SlashCommandMixer::new();
    // All sources share the StaticSlashCommands filter because the mixer only runs
    // async sources when the query's filters intersect with the source's filters.
    mixer.add_sync_source(
        slash_commands_source.clone(),
        [QueryFilter::StaticSlashCommands],
    );
    mixer.add_async_source(
        super::saved_prompts_data_source(),
        [QueryFilter::StaticSlashCommands],
        AddAsyncSourceOptions {
            // Any debounce makes the loading state flicker longer.
            debounce_interval: None,
            run_in_zero_state: false,
            run_when_unfiltered: false,
        },
        ctx,
    );
    mixer.add_sync_source(
        ZeroStateDataSource::new(&slash_commands_source, is_cloud_mode_v2),
        [QueryFilter::StaticSlashCommands],
    );
    mixer.run_query(slash_command_query(""), ctx);
    mixer
}

pub fn slash_command_query(text: &str) -> Query {
    Query {
        text: text.to_owned(),
        filters: [QueryFilter::StaticSlashCommands].into(),
    }
}
