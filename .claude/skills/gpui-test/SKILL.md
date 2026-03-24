# GPUI Testing Skill

This skill provides testing patterns and best practices for GPUI applications in CloudBridge.

## When to Use

Use this skill when:
- Writing unit tests for GPUI components
- Testing async operations in UI
- Creating integration tests
- Need test examples for cloud providers

## GPUI Test Basics

### Setup

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext};

    #[gpui::test]
    fn test_basic_component(cx: &mut TestAppContext) {
        let view = cx.new_view(|cx| MyView::new(cx));
        // Assertions
    }
}
```

### Testing State Changes

```rust
#[gpui::test]
fn test_state_update(cx: &mut TestAppContext) {
    let view = cx.new_view(|cx| CounterView::new(cx));

    view.update(cx, |view, cx| {
        assert_eq!(view.count, 0);
        view.increment(cx);
        assert_eq!(view.count, 1);
    });
}
```

### Testing Async Operations

```rust
#[gpui::test]
async fn test_async_data_loading(cx: &mut TestAppContext) {
    let view = cx.new_view(|cx| DataView::new(cx));

    view.update(cx, |view, cx| {
        view.load_data(cx);
    });

    // Wait for async operation
    cx.run_until_parked();

    view.update(cx, |view, _| {
        assert!(view.data.is_some());
    });
}
```

## Testing CloudBridge Components

### 1. Testing Crypto Operations

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encrypt_decrypt() {
        let key = generate_key();
        let plaintext = "sensitive-data";

        let encrypted = encrypt(plaintext, &key).unwrap();
        let decrypted = decrypt(&encrypted, &key).unwrap();

        assert_eq!(plaintext, decrypted);
    }

    #[test]
    fn test_encryption_with_different_key_fails() {
        let key1 = generate_key();
        let key2 = generate_key();
        let plaintext = "data";

        let encrypted = encrypt(plaintext, &key1).unwrap();
        let result = decrypt(&encrypted, &key2);

        assert!(result.is_err());
    }
}
```

### 2. Testing Cloud Providers

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use mockito::{Server, Mock};

    #[test]
    fn test_aws_cost_parsing() {
        let json = r#"{
            "ResultsByTime": [{
                "TimePeriod": {"Start": "2024-01-01", "End": "2024-01-31"},
                "Total": {
                    "UnblendedCost": {"Amount": "100.50", "Unit": "USD"}
                }
            }]
        }"#;

        let result = parse_aws_cost_response(json).unwrap();
        assert_eq!(result.total_cost, 100.50);
    }

    #[tokio::test]
    async fn test_aws_api_call_with_mock() {
        let mut server = Server::new();
        let mock = server.mock("POST", "/")
            .with_status(200)
            .with_body(r#"{"ResultsByTime": []}"#)
            .create();

        let client = AwsClient::new_with_endpoint(&server.url());
        let result = client.get_cost_data("2024-01-01", "2024-01-31").await;

        assert!(result.is_ok());
        mock.assert();
    }
}
```

### 3. Testing Database Operations

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn setup_test_db() -> (Connection, TempDir) {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.duckdb");
        let conn = Connection::open(&db_path).unwrap();
        init_database(&conn).unwrap();
        (conn, temp_dir)
    }

    #[test]
    fn test_insert_and_retrieve_account() {
        let (conn, _temp) = setup_test_db();

        let account = CloudAccount {
            name: "test-aws".to_string(),
            provider: "aws".to_string(),
            access_key: "encrypted-key".to_string(),
            secret_key: "encrypted-secret".to_string(),
        };

        insert_account(&conn, &account).unwrap();
        let retrieved = get_account_by_name(&conn, "test-aws").unwrap();

        assert_eq!(retrieved.name, account.name);
        assert_eq!(retrieved.provider, account.provider);
    }

    #[test]
    fn test_cache_expiration() {
        let (conn, _temp) = setup_test_db();

        // Insert cache entry
        let summary = CostSummary { /* ... */ };
        cache_cost_summary(&conn, "account-1", &summary).unwrap();

        // Immediately should be valid
        assert!(is_cache_valid(&conn, "account-1", 6).unwrap());

        // Simulate time passing (in real test, use time mocking)
        // For now, test with 0 hour TTL
        assert!(!is_cache_valid(&conn, "account-1", 0).unwrap());
    }
}
```

### 4. Testing UI Components

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn test_dashboard_view_creation(cx: &mut TestAppContext) {
        let view = cx.new_view(|cx| DashboardView::new(cx));
        assert!(view.is_ok());
    }

    #[gpui::test]
    async fn test_refresh_button_click(cx: &mut TestAppContext) {
        let view = cx.new_view(|cx| DashboardView::new(cx));

        view.update(cx, |view, cx| {
            // Simulate refresh button click
            view.refresh(cx);
            assert_eq!(view.loading, true);
        });

        cx.run_until_parked();

        view.update(cx, |view, _| {
            // After loading completes
            assert_eq!(view.loading, false);
        });
    }
}
```

## Test Organization

### File Structure

```
src/
├── crypto.rs
├── crypto_tests.rs      # or #[cfg(test)] mod in crypto.rs
├── cloud/
│   ├── aws.rs
│   ├── aws_tests.rs
│   ├── aliyun.rs
│   └── aliyun_tests.rs
└── ui/
    ├── dashboard.rs
    └── dashboard_tests.rs
```

### Integration Tests

```
tests/
├── integration_test.rs
├── fixtures/
│   ├── aws_response.json
│   └── aliyun_response.json
└── helpers/
    └── mod.rs
```

## Common Test Patterns

### 1. Parameterized Tests

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_multiple_currencies() {
        let test_cases = vec![
            ("100.00", "USD", 100.0),
            ("50.50", "EUR", 50.5),
            ("1000", "CNY", 1000.0),
        ];

        for (amount, currency, expected) in test_cases {
            let result = parse_cost(amount, currency).unwrap();
            assert_eq!(result, expected);
        }
    }
}
```

### 2. Testing Error Conditions

```rust
#[test]
fn test_invalid_credentials() {
    let client = AwsClient::new("invalid-key", "invalid-secret");
    let result = client.validate_credentials();

    assert!(result.is_err());
    assert!(matches!(result, Err(CloudError::InvalidCredentials)));
}

#[test]
#[should_panic(expected = "encryption key must be 32 bytes")]
fn test_invalid_key_length_panics() {
    let short_key = vec![0u8; 16]; // Too short
    encrypt("data", &short_key).unwrap();
}
```

### 3. Using Test Fixtures

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn load_fixture(name: &str) -> String {
        std::fs::read_to_string(
            format!("tests/fixtures/{}.json", name)
        ).unwrap()
    }

    #[test]
    fn test_parse_aws_response() {
        let json = load_fixture("aws_cost_response");
        let result = parse_aws_response(&json).unwrap();

        assert_eq!(result.len(), 3);
        assert_eq!(result[0].service_name, "EC2");
    }
}
```

### 4. Mocking External Dependencies

```rust
#[cfg(test)]
mod tests {
    use super::*;

    // Mock HTTP client
    struct MockHttpClient {
        responses: Vec<String>,
    }

    impl HttpClient for MockHttpClient {
        fn post(&self, url: &str, body: &str) -> Result<String> {
            Ok(self.responses[0].clone())
        }
    }

    #[test]
    fn test_with_mock_http() {
        let mock_client = MockHttpClient {
            responses: vec![r#"{"status": "ok"}"#.to_string()],
        };

        let service = MyService::new(Box::new(mock_client));
        let result = service.fetch_data().unwrap();

        assert_eq!(result.status, "ok");
    }
}
```

## Running Tests

```bash
# Run all tests
cargo test

# Run specific test
cargo test test_encrypt_decrypt

# Run tests with output
cargo test -- --nocapture

# Run tests in specific module
cargo test crypto::tests

# Run tests matching pattern
cargo test aws_

# Run with coverage (requires tarpaulin)
cargo tarpaulin --out Html
```

## Best Practices

1. **Test one thing per test** - Keep tests focused and simple
2. **Use descriptive test names** - `test_encryption_fails_with_wrong_key` vs `test_1`
3. **Arrange-Act-Assert** - Clear structure in tests
4. **Clean up resources** - Use `Drop` trait or `TempDir` for cleanup
5. **Don't test third-party code** - Test your own logic
6. **Make tests deterministic** - Avoid time-based or random tests
7. **Test edge cases** - Empty inputs, maximum values, invalid data
8. **Use test helpers** - DRY principle applies to tests too

## Example: Complete Test Module

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    // Helper functions
    fn create_test_account() -> CloudAccount {
        CloudAccount {
            name: "test".to_string(),
            provider: "aws".to_string(),
            access_key: "key".to_string(),
            secret_key: "secret".to_string(),
        }
    }

    // Unit tests
    #[test]
    fn test_account_creation() {
        let account = create_test_account();
        assert_eq!(account.name, "test");
    }

    // Async tests
    #[tokio::test]
    async fn test_async_operation() {
        let result = fetch_data().await;
        assert!(result.is_ok());
    }

    // GPUI tests
    #[gpui::test]
    fn test_ui_component(cx: &mut TestAppContext) {
        let view = cx.new_view(|cx| MyView::new(cx));
        view.update(cx, |view, cx| {
            assert!(view.initialized);
        });
    }
}
```
