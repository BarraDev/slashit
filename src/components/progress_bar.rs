use leptos::prelude::*;

#[component]
pub fn ProgressBar(progress: u8, label: Option<String>) -> impl IntoView {
    let clamped_progress = progress.min(100);
    let color = if clamped_progress >= 80 {
        "#10b981"
    } else if clamped_progress >= 50 {
        "#3b82f6"
    } else if clamped_progress >= 25 {
        "#f59e0b"
    } else {
        "#6b7280"
    };

    view! {
        <div class="progress-bar-container">
            <div class="progress-bar">
                <div
                    class="progress-fill"
                    style=format!("width: {}%; background-color: {}", clamped_progress, color)
                ></div>
            </div>
            <span class="progress-text">
                {label.unwrap_or_else(|| format!("{}%", clamped_progress))}
            </span>
        </div>
    }
}
