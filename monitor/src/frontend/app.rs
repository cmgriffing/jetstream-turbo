use crate::stats::StreamStats;
use leptos::prelude::*;
use leptos_axum::generate_route_list;

#[component]
pub fn App() -> impl IntoView {
    let (stats, set_stats) = signal(StreamStats {
        stream_a: 0,
        stream_b: 0,
        delta: 0,
        rate_a: 0.0,
        rate_b: 0.0,
        timestamp: chrono::Utc::now(),
    });
    
    let count_a = move || stats.read().stream_a;
    let count_b = move || stats.read().stream_b;
    let rate_a = move || stats.read().rate_a;
    let rate_b = move || stats.read().rate_b;
    let delta = move || stats.read().delta;
    
    let delta_class = move || {
        let d = stats.read().delta;
        if d > 0 { "delta positive" }
        else if d < 0 { "delta negative" }
        else { "delta neutral" }
    };

    Effect::new(move |_| {
        let set_stats = set_stats;
        
        leptos::task::spawn_local(async move {
            use web_sys::WebSocket;
            use futures::StreamExt;
            
            let ws = WebSocket::new("/ws").unwrap();
            let mut recv = futures::stream::unfold(ws.clone(), |ws| async move {
                loop {
                    let msg = ws.recv().await.ok()?;
                    if let web_sys::MessageEvent::Text(text) = msg {
                        return Some((text, ws));
                    }
                }
            });
            
            while let Some(text) = recv.next().await {
                if let Ok(new_stats) = serde_json::from_str::<StreamStats>(&text) {
                    set_stats.set(new_stats);
                }
            }
        });
    });

    view! {
        <!DOCTYPE html>
        <html lang="en">
            <head>
                <meta charset="utf-8"/>
                <meta name="viewport" content="width=device-width, initial-scale=1"/>
                <title>"Bluesky Stream Monitor"</title>
                <style>
                    {r#"
                    * { box-sizing: border-box; margin: 0; padding: 0; }
                    body {
                        font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif;
                        background: #0a0a0a;
                        color: #e0e0e0;
                        min-height: 100vh;
                        display: flex;
                        flex-direction: column;
                        align-items: center;
                        justify-content: center;
                        padding: 2rem;
                    }
                    .container {
                        display: grid;
                        grid-template-columns: 1fr auto 1fr;
                        gap: 2rem;
                        align-items: center;
                        max-width: 800px;
                    }
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
                    .header {
                        text-align: center;
                        margin-bottom: 2rem;
                    }
                    .header h1 {
                        font-size: 1.5rem;
                        font-weight: 600;
                        margin-bottom: 0.25rem;
                    }
                    .header p {
                        color: #666;
                        font-size: 0.875rem;
                    }
                    "#}
                </style>
            </head>
            <body>
                <div class="header">
                    <h1>"Stream Monitor"</h1>
                    <p>"Comparing Bluesky turbo streams"</p>
                </div>
                <div class="container">
                    <div class="counter">
                        <div class="label">"Stream A"</div>
                        <div class="value">{count_a}</div>
                        <div class="rate">{move || format!("{:.0}/s", rate_a())}</div>
                    </div>
                    <div class=delta_class>
                        <div class="label">"Delta"</div>
                        <div class="value">
                            {move || {
                                let d = delta();
                                if d > 0 { format!("+{}", d) }
                                else { d.to_string() }
                            }}
                        </div>
                    </div>
                    <div class="counter">
                        <div class="label">"Stream B"</div>
                        <div class="value">{count_b}</div>
                        <div class="rate">{move || format!("{:.0}/s", rate_b())}</div>
                    </div>
                </div>
                <script>
                    {r#"
                    const ws = new WebSocket(location.origin.replace('http', 'ws') + '/ws');
                    ws.onmessage = (e) => {
                        const stats = JSON.parse(e.data);
                        const el = document.getElementById('stats-data');
                        if (el) el.textContent = JSON.stringify(stats);
                    };
                    "#}
                </script>
                <script type="module">
                    {r#"
                    import { hydrate } from '/pkg/jetstream_monitor.js';
                    hydrate();
                    "#}
                </script>
            </body>
        </html>
    }
}

pub fn get_routes() -> Vec<RouteListing> {
    generate_route_list(App)
}
