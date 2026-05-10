use leptos::prelude::*;

#[component]
pub fn Badge(text: String, color: String) -> impl IntoView {
    view! {
        <span class="badge" style=format!("background-color: {}", color)>
            {text}
        </span>
    }
}

#[component]
pub fn CategoryBadge(category: crate::models::TaskCategory) -> impl IntoView {
    let (text, color) = match category {
        crate::models::TaskCategory::Feature => ("Feature", "#3b82f6"),
        crate::models::TaskCategory::BugFix => ("Bug Fix", "#ef4444"),
        crate::models::TaskCategory::Refactoring => ("Refactoring", "#8b5cf6"),
        crate::models::TaskCategory::Documentation => ("Docs", "#06b6d4"),
        crate::models::TaskCategory::Security => ("Security", "#f59e0b"),
        crate::models::TaskCategory::Performance => ("Performance", "#10b981"),
        crate::models::TaskCategory::UiUx => ("UI/UX", "#ec4899"),
        crate::models::TaskCategory::Infrastructure => ("Infra", "#6b7280"),
        crate::models::TaskCategory::Testing => ("Testing", "#14b8a6"),
    };

    view! {
        <Badge text=text.to_string() color=color.to_string() />
    }
}

#[component]
pub fn PriorityBadge(priority: crate::models::TaskPriority) -> impl IntoView {
    let (text, color) = match priority {
        crate::models::TaskPriority::Urgent => ("Urgent", "#dc2626"),
        crate::models::TaskPriority::High => ("High", "#ea580c"),
        crate::models::TaskPriority::Medium => ("Medium", "#d97706"),
        crate::models::TaskPriority::Low => ("Low", "#6b7280"),
    };

    view! {
        <Badge text=text.to_string() color=color.to_string() />
    }
}

#[component]
pub fn ComplexityBadge(complexity: crate::models::TaskComplexity) -> impl IntoView {
    let (text, color) = match complexity {
        crate::models::TaskComplexity::Minimal => ("Minimal", "#10b981"),
        crate::models::TaskComplexity::Moderate => ("Moderate", "#3b82f6"),
        crate::models::TaskComplexity::Complex => ("Complex", "#f59e0b"),
        crate::models::TaskComplexity::Advanced => ("Advanced", "#ef4444"),
    };

    view! {
        <Badge text=text.to_string() color=color.to_string() />
    }
}

#[component]
pub fn ImpactBadge(impact: crate::models::TaskImpact) -> impl IntoView {
    let (text, color) = match impact {
        crate::models::TaskImpact::Low => ("Low Impact", "#6b7280"),
        crate::models::TaskImpact::Medium => ("Medium Impact", "#3b82f6"),
        crate::models::TaskImpact::High => ("High Impact", "#f59e0b"),
        crate::models::TaskImpact::Critical => ("Critical", "#dc2626"),
    };

    view! {
        <Badge text=text.to_string() color=color.to_string() />
    }
}

#[component]
pub fn SecuritySeverityBadge(severity: crate::models::SecuritySeverity) -> impl IntoView {
    let (text, color) = match severity {
        crate::models::SecuritySeverity::None => ("", ""),
        crate::models::SecuritySeverity::Low => ("Low Sec", "#10b981"),
        crate::models::SecuritySeverity::Medium => ("Med Sec", "#f59e0b"),
        crate::models::SecuritySeverity::High => ("High Sec", "#ea580c"),
        crate::models::SecuritySeverity::Critical => ("Critical Sec", "#dc2626"),
    };

    if text.is_empty() {
        ().into_any()
    } else {
        view! {
            <Badge text=text.to_string() color=color.to_string() />
        }.into_any()
    }
}
