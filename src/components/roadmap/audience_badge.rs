use leptos::prelude::*;

#[component]
pub fn AudienceBadge(audience: crate::models::TargetAudience) -> impl IntoView {
    let (icon, color, label) = match audience {
        crate::models::TargetAudience::Technical => ("👨‍💻", "#3b82f6", "Technical"),
        crate::models::TargetAudience::Business => ("💼", "#8b5cf6", "Business"),
        crate::models::TargetAudience::EndUser => ("👤", "#10b981", "End User"),
    };

    view! {
        <span class="audience-badge" style=format!("background-color: {}; color: white", color)>
            <span class="audience-icon">{icon}</span>
            <span class="audience-label">{label}</span>
        </span>
    }
}
