# jetstream-turbo
![Image from 391 Vol 1– 19 by Francis Picabia, https://archive.org/details/391-vol-1-19/page/n24/mode/1up](./jetstream-turbo.png)
Turbocharged Jetstream messages - hydrates referenced objects, stores to SQLite, punches up to S3 for long term storage.

## Scratch

1. `docker compose build && docker compose up`
2. `docker exec -it jetstream_turbo_service bash`
3. `pdm run turbocharger`

Note: Will almost certainly require round robining session strings. A to-do though everything else is now playing nicely.
