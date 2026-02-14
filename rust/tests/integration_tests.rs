#[cfg(test)]
mod tests {
    use jetstream_turbo_rs::client::{BlueskyAuthClient, JetstreamClient};
    use jetstream_turbo_rs::config::Settings;
    use jetstream_turbo_rs::hydration::TurboCache;
    use jetstream_turbo_rs::models::{bluesky::BlueskyProfile, jetstream::JetstreamMessage};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn test_configuration_loading() {
        // Test default configuration
        let settings = Settings::default();
        assert_eq!(settings.wanted_collections, "app.bsky.feed.post");
        assert_eq!(settings.batch_size, 10);
        assert!(settings.jetstream_hosts.len() > 0);
    }

    #[tokio::test]
    async fn test_bluesky_auth_integration() {
        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/com.atproto.server.createSession"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "accessJwt": "test_jwt_token",
                "refreshJwt": "test_refresh_token",
                "handle": "test.bsky.social",
                "did": "did:plc:test"
            })))
            .mount(&mock_server)
            .await;

        let client = BlueskyAuthClient::with_api_url(
            "test.bsky.social".to_string(),
            "test-app-password".to_string(),
            mock_server.uri(),
        );

        let session = client.authenticate().await.unwrap();
        assert!(session.contains("test_jwt_token"));
    }

    #[tokio::test]
    async fn test_jetstream_client_message_parsing() {
        let endpoints = vec!["test.bsky.network".to_string()];
        let client = JetstreamClient::new(endpoints, "app.bsky.feed.post".to_string());

        let valid_json = r#"
        {
            "did": "did:plc:test",
            "seq": 12345,
            "time_us": 1640995200000000,
            "kind": "commit",
            "commit": {
                "rev": "test-rev",
                "operation": "create",
                "collection": "app.bsky.feed.post",
                "rkey": "test",
                "record": {
                    "uri": "at://did:plc:test/app.bsky.feed.post/test",
                    "cid": "bafyrei",
                    "author": "did:plc:test",
                    "type": "app.bsky.feed.post",
                    "createdAt": "2022-01-01T00:00:00.000Z",
                    "text": "test"
                }
            }
        }
        "#;

        let result = client.parse_message(valid_json);
        assert!(result.is_ok());

        let message = result.unwrap();
        assert_eq!(message.did, "did:plc:test");
        assert_eq!(message.seq, Some(12345));
    }

    #[tokio::test]
    async fn test_cache_performance() {
        let cache = TurboCache::new(10000, 10000);

        let profile = BlueskyProfile {
            did: "did:plc:test".into(),
            handle: "test.bsky.social".to_string(),
            display_name: Some("Test User".to_string()),
            description: None,
            avatar: None,
            banner: None,
            followers_count: Some(1000),
            follows_count: Some(500),
            posts_count: Some(250),
            indexed_at: None,
            created_at: None,
            labels: None,
        };

        // Benchmark cache operations
        let start = std::time::Instant::now();

        for i in 0..10000 {
            cache
                .set_user_profile(
                    format!("did:plc:test{}", i),
                    std::sync::Arc::new(profile.clone()),
                )
                .await;
        }

        let set_time = start.elapsed();

        let start = std::time::Instant::now();

        for i in 0..10000 {
            let _result = cache.get_user_profile(&format!("did:plc:test{}", i)).await;
        }

        let get_time = start.elapsed();

        println!("Cache set time for 10k items: {:?}", set_time);
        println!("Cache get time for 10k items: {:?}", get_time);

        // Verify cache hit rates
        let (user_hit_rate, post_hit_rate) = cache.get_hit_rates().await;
        assert_eq!(user_hit_rate, 1.0); // All should be hits
        assert_eq!(post_hit_rate, 0.0); // No post operations

        // Performance assertions (these should be very fast)
        assert!(set_time.as_millis() < 1000); // Less than 1 second
        assert!(get_time.as_millis() < 500); // Less than 500ms
    }

    #[tokio::test]
    async fn test_message_extraction() {
        let message_json = r#"
        {
            "did": "did:plc:alice",
            "seq": 12345678901234567890,
            "time_us": 1704067200000000,
            "kind": "commit",
            "commit": {
                "rev": "test-rev",
                "operation": "create",
                "collection": "app.bsky.feed.post",
                "rkey": "3jk7v7mjpxq3y",
                "record": {
                    "$type": "app.bsky.feed.post",
                    "text": "Hello @bob.example.com! Check out this post at://did:plc:charlie/app.bsky.feed.post/xyz #test",
                    "createdAt": "2024-01-01T00:00:00.000Z",
                    "facets": [
                        {
                            "index": {"byteStart": 6, "byteEnd": 23},
                            "features": [{"$type": "app.bsky.richtext.facet#mention", "did": "did:plc:bob"}]
                        },
                        {
                            "index": {"byteStart": 44, "byteEnd": 97},
                            "features": [{"$type": "app.bsky.richtext.facet#link", "uri": "at://did:plc:charlie/app.bsky.feed.post/xyz"}]
                        },
                        {
                            "index": {"byteStart": 98, "byteEnd": 103},
                            "features": [{"$type": "app.bsky.richtext.facet#tag", "tag": "test"}]
                        }
                    ]
                },
                "cid": "bafyreib5qtxdqevgwz2xcvxfzkbizsu2gswfkcayn3mpfr2a24jkmvotgi"
            }
        }
        "#;

        let message: JetstreamMessage = serde_json::from_str(message_json).unwrap();

        // Test message extraction
        assert_eq!(message.extract_did(), "did:plc:alice");
        assert_eq!(
            message.extract_at_uri(),
            Some("at://did:plc:alice/app.bsky.feed.post/3jk7v7mjpxq3y".to_string())
        );

        // Test DID extraction from mentions
        let mentioned_dids = message.extract_mentioned_dids();
        assert!(mentioned_dids.contains(&"did:plc:bob"));
        assert!(!mentioned_dids.is_empty());
    }

    #[tokio::test]
    async fn test_end_to_end_scenario() {
        // This test simulates a mini end-to-end flow

        // 1. Setup cache
        let cache = TurboCache::new(1000, 1000);

        // 2. Simulate message reception
        let messages = vec![JetstreamMessage {
            did: "did:plc:user1".to_string(),
            seq: Some(1),
            time_us: Some(1704067200000000),
            kind: "commit".to_string(),
            commit: Some(jetstream_turbo_rs::models::jetstream::CommitData {
                rev: Some("test-rev".to_string()),
                operation_type: "create".to_string(),
                collection: Some("app.bsky.feed.post".to_string()),
                rkey: Some("1".to_string()),
                record: Some(serde_json::json!({"text": "Hello world"})),
                cid: Some("cid1".to_string()),
            }),
        }];

        // 3. Process messages and simulate hydration
        for message in messages {
            let author_did = message.extract_did();

            // Check if we have the author profile cached
            let profile = cache.get_user_profile(author_did).await;

            if profile.is_none() {
                // Simulate API call and cache result
                let new_profile = BlueskyProfile {
                    did: author_did.into(),
                    handle: format!("{}.bsky.social", &author_did[9..13]), // Extract part for demo
                    display_name: Some("User 1".to_string()),
                    description: None,
                    avatar: None,
                    banner: None,
                    followers_count: Some(100),
                    follows_count: Some(50),
                    posts_count: Some(25),
                    indexed_at: None,
                    created_at: None,
                    labels: None,
                };

                cache
                    .set_user_profile(author_did.to_string(), std::sync::Arc::new(new_profile))
                    .await;
            }
        }

        // 4. Verify results
        let final_profile = cache.get_user_profile("did:plc:user1").await;
        assert!(final_profile.is_some());
        assert_eq!(
            final_profile.unwrap().display_name,
            Some("User 1".to_string())
        );

        // 5. Check metrics
        let metrics = cache.get_metrics().await;
        assert!(metrics.user_hits > 0);
        assert!(metrics.user_misses > 0);
    }
}
