//! Test-only fixture plugin. Exists purely so `ut-plugin-loader`'s tests
//! can `dlopen` a *real* compiled cdylib built against
//! `ut_plugin_loader::export_plugin!`, instead of asserting the macro
//! expands to something plausible-looking. Not published, not referenced
//! by any non-test code.

use ut_plugin_api::PluginAction;
use ut_plugin_loader::{export_plugin, DynPlugin};

#[derive(Default)]
struct EchoFixture;

impl DynPlugin for EchoFixture {
    fn id(&self) -> String {
        "echo-fixture".to_string()
    }

    fn on_command(&mut self, command: &str) -> PluginAction {
        PluginAction::SuggestCommand(format!("echo: {command}"))
    }

    fn on_output(&mut self, chunk: &str) -> PluginAction {
        PluginAction::ShowMessage(format!("saw output: {chunk}"))
    }
}

export_plugin!(EchoFixture);
