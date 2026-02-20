use crate::frontend::components::{Counter, Delta};
use crate::stats::StreamStats;
use leptos::prelude::*;
use leptos_axum::generate_route_list;

const BASE_CSS: &str = r#"
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
"#;

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

    let count_a = Signal::derive(move || stats.read().stream_a);
    let count_b = Signal::derive(move || stats.read().stream_b);
    let rate_a = Signal::derive(move || stats.read().rate_a);
    let rate_b = Signal::derive(move || stats.read().rate_b);
    let delta = Signal::derive(move || stats.read().delta);

    Effect::new(move |_| {
        let set_stats = set_stats;

        leptos::task::spawn_local(async move {
            use futures::StreamExt;
            use web_sys::WebSocket;

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
                <style>{BASE_CSS}</style>
            </head>
            <body>
                <div class="header">
                    <h1>"Stream Monitor"</h1>
                    <p>"Comparing Bluesky turbo streams"</p>
                </div>
                <div class="container">
                    <Counter label="Stream A" count=count_a rate=rate_a />
                    <Delta delta=delta />
                    <Counter label="Stream B" count=count_b rate=rate_b />
                </div>
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
