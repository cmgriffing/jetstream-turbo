use leptos::prelude::*;

const COUNTER_CSS: &str = r#"
.counter {
    background: #1a1a1a;
    border-radius: 12px;
    padding: 2rem;
    text-align: center;
    border: 1px solid #333;
}
.counter .label {
    font-size: 0.875rem;
    color: #888;
    margin-bottom: 0.5rem;
}
.counter .value {
    font-size: 2.5rem;
    font-weight: 700;
    font-variant-numeric: tabular-nums;
}
.counter .rate {
    font-size: 1rem;
    color: #666;
    margin-top: 0.5rem;
}
"#;

const DELTA_CSS: &str = r#"
.delta {
    display: flex;
    flex-direction: column;
    align-items: center;
    padding: 1rem 2rem;
}
.delta .label {
    font-size: 0.75rem;
    color: #666;
}
.delta .value {
    font-size: 2rem;
    font-weight: 600;
}
.delta.positive .value { color: #4ade80; }
.delta.negative .value { color: #f87171; }
.delta.neutral .value { color: #888; }
"#;

#[component]
pub fn Counter(label: &'static str, count: Signal<u64>, rate: Signal<f64>) -> impl IntoView {
    view! {
        <style>{COUNTER_CSS}</style>
        <div class="counter">
            <div class="label">{label}</div>
            <div class="value">{move || count.get()}</div>
            <div class="rate">{move || format!("{:.0}/s", rate.get())}</div>
        </div>
    }
}

#[component]
pub fn Delta(delta: Signal<i64>) -> impl IntoView {
    let class = Signal::derive(move || {
        let d = delta.get();
        if d > 0 {
            "delta positive"
        } else if d < 0 {
            "delta negative"
        } else {
            "delta neutral"
        }
    });

    view! {
        <style>{DELTA_CSS}</style>
        <div class={move || class.get()}>
            <div class="label">"Delta"</div>
            <div class="value">
                {move || {
                    let d = delta.get();
                    if d > 0 { format!("+{}", d) }
                    else { d.to_string() }
                }}
            </div>
        </div>
    }
}
