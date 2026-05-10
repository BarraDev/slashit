use leptos::prelude::*;
use crate::models::Project;

#[component]
pub fn ProjectCard(project: Project) -> impl IntoView {
    let agent_type_badge = |agent_type: &crate::models::AgentType| -> (String, &'static str) {
        match agent_type {
            crate::models::AgentType::ClaudeCode => ("Claude Code".to_string(), "bg-purple-500/20 text-purple-300"),
            crate::models::AgentType::Cursor => ("Cursor".to_string(), "bg-blue-500/20 text-blue-300"),
            crate::models::AgentType::Cody => ("Cody".to_string(), "bg-green-500/20 text-green-300"),
            crate::models::AgentType::Continue => ("Continue".to_string(), "bg-orange-500/20 text-orange-300"),
            crate::models::AgentType::Other(name) => (name.clone(), "bg-gray-500/20 text-gray-300"),
        }
    };

    view! {
        <div class="group border border-white/10 rounded-xl bg-white/[0.02] hover:border-white/20 hover:bg-white/[0.04] transition-all duration-200 cursor-pointer">
            <div class="p-4">
                <div class="flex items-start justify-between gap-3">
                    <div class="flex-1 min-w-0">
                        <h3 class="font-semibold text-white/90 truncate group-hover:text-white transition-colors">
                            {project.name}
                        </h3>
                        <p class="text-xs text-white/40 mt-1 font-mono truncate">
                            {format!("ID: {}", project.id.to_string().split_at(8).0)}
                        </p>
                    </div>

                    <div class="flex flex-col items-end gap-2">
                        <span class=format!(
                            "px-2 py-1 rounded text-xs font-medium {}",
                            agent_type_badge(&project.agent_type).1
                        )>
                            {agent_type_badge(&project.agent_type).0}
                        </span>
                    </div>
                </div>

                <div class="flex items-center gap-4 mt-4 pt-3 border-t border-white/5">
                    <div class="flex items-center gap-1.5 text-xs text-white/40">
                        <svg class="w-3.5 h-3.5" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                            <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M8 7V3m8 4V3m-9 8h10M5 21h14a2 2 0 002-2V7a2 2 0 00-2-2H5a2 2 0 00-2 2v12a2 2 0 002 2z" />
                        </svg>
                        <span>{project.created_at.format("%Y-%m-%d").to_string()}</span>
                    </div>

                    <div class="flex items-center gap-1.5 text-xs text-white/40">
                        <svg class="w-3.5 h-3.5" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                            <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M7 7h.01M7 3h5c.512 0 1.024.195 1.414.586l7 7a2 2 0 010 2.828l-7 7a2 2 0 01-2.828 0l-7-7A1.994 1.994 0 013 12V7a4 4 0 014-4z" />
                        </svg>
                        <span>{format!("{:?}", project.id).split_at(8).0.to_string()}</span>
                    </div>
                </div>
            </div>
        </div>
    }
}
