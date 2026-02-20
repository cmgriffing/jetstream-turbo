use leptos::*;

#[component]
pub fn Counter(
    label: &'static str,
    count: ReadSignal<u64>,
    rate: ReadSignal<f64>,
) -> impl IntoView {
    view! {
        <div class="counter">
            <div class="label">{label}</div>
            <div class="value">{move || count.get().to_string()}</div>
            <div class="rate">
                {move || format!("{:.0}/s", rate.get())}
            </div>
        </div>
    }
}

#[component]
pub fn Delta(delta: ReadSignal<i64>) -> impl IntoView {
    let class = move || {
        if delta.get() > 0 {
            "delta positive"
        } else if delta.get() < 0 {
            "delta negative"
        } else {
            "delta neutral"
        }
    };

    let prefix = move || if delta.get() > 0 { "+" } else { "" };

    view! {
        <div class={class}>
            <span class="label">"Delta"</span>
            <span class="value">
                {move || format!("{}{}", prefix(), delta.get())}
            </span>
        </div>
    }
}
