use leptos::prelude::*;

#[component]
pub fn PhaseBadge(phase: crate::models::TaskPhase, progress: u8) -> impl IntoView {
    let (icon, color, label) = match phase {
        crate::models::TaskPhase::Idle => ("💤", "#6b7280", "Idle"),
        crate::models::TaskPhase::Planning => ("📋", "#3b82f6", "Planning"),
        crate::models::TaskPhase::Coding => ("💻", "#10b981", "Coding"),
        crate::models::TaskPhase::QaReview => ("🔍", "#8b5cf6", "QA Review"),
        crate::models::TaskPhase::QaFixing => ("🔧", "#f59e0b", "QA Fixing"),
        crate::models::TaskPhase::Complete => ("✅", "#10b981", "Complete"),
        crate::models::TaskPhase::Failed => ("❌", "#ef4444", "Failed"),
    };

    view! {
        <div class="phase-badge" style=format!("border-color: {}", color)>
            <span class="phase-icon">{icon}</span>
            <span class="phase-label">{label}</span>
            <span class="phase-progress" style=format!("color: {}", color)>
                {format!("{}%", progress)}
            </span>
        </div>
    }
}
