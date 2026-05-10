use leptos::prelude::*;

#[component]
pub fn SkeletonCard() -> impl IntoView {
    view! {
        <div class="border border-white/10 rounded-xl bg-white/[0.02] overflow-hidden animate-pulse">
            <div class="p-4 space-y-3">
                <div class="flex items-center gap-2">
                    <div class="w-3 h-3 rounded-full bg-white/10"></div>
                    <div class="h-4 bg-white/10 rounded w-1/3"></div>
                </div>
                <div class="space-y-2">
                    <div class="h-3 bg-white/10 rounded w-full"></div>
                    <div class="h-3 bg-white/10 rounded w-2/3"></div>
                </div>
                <div class="flex items-center justify-between pt-2">
                    <div class="h-3 bg-white/10 rounded w-20"></div>
                    <div class="h-8 bg-white/10 rounded w-20"></div>
                </div>
            </div>
        </div>
    }
}

#[component]
pub fn SkeletonTask() -> impl IntoView {
    view! {
        <div class="border border-white/10 rounded-xl bg-white/[0.02] p-4 animate-pulse">
            <div class="space-y-3">
                <div class="flex items-center justify-between">
                    <div class="h-4 bg-white/10 rounded w-1/2"></div>
                    <div class="h-3 bg-white/10 rounded w-16"></div>
                </div>
                <div class="space-y-2">
                    <div class="h-3 bg-white/10 rounded w-full"></div>
                    <div class="h-3 bg-white/10 rounded w-3/4"></div>
                </div>
            </div>
        </div>
    }
}

#[component]
pub fn SkeletonRow() -> impl IntoView {
    view! {
        <div class="flex items-center gap-4 p-4 border border-white/10 rounded-lg animate-pulse">
            <div class="w-10 h-10 rounded-full bg-white/10"></div>
            <div class="flex-1 space-y-2">
                <div class="h-4 bg-white/10 rounded w-1/3"></div>
                <div class="h-3 bg-white/10 rounded w-2/3"></div>
            </div>
            <div class="h-8 bg-white/10 rounded w-20"></div>
        </div>
    }
}
