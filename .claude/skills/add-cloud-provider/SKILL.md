# Add Cloud Provider Skill

This skill guides you through adding a new cloud provider to CloudBridge.

## When to Use

Use this skill when:
- Adding support for a new cloud platform (Azure, GCP, etc.)
- Implementing a new cost API integration
- Extending CloudBridge with custom providers

## Overview

Adding a new cloud provider involves:
1. Implementing the `CloudService` trait
2. Adding authentication/signing logic
3. Creating API request/response handlers
4. Adding UI configuration options
5. Testing the integration

## Step-by-Step Guide

### Step 1: Review Existing Providers

CloudBridge currently supports:
- **AWS** - `src/cloud/aws.rs` (758 lines)
  - AWS Signature V4 authentication
  - Cost Explorer API

- **Alibaba Cloud** - `src/cloud/aliyun.rs` (480 lines)
  - HMAC-SHA1 signature
  - Billing API

- **DeepSeek** - `src/cloud/deepseek.rs` (147 lines)
  - Simple API key auth
  - Balance queries

### Step 2: Understand the CloudService Trait

```rust
// src/cloud/mod.rs
pub trait CloudService: Send + Sync {
    /// Validate if the credentials are correct
    fn validate_credentials(&self) -> Result<bool, String>;

    /// Get cost data for a specific date range
    fn get_cost_data(&self, start_date: &str, end_date: &str)
        -> Result<Vec<CostData>, String>;

    /// Get monthly cost summary with service breakdown
    fn get_cost_summary(&self) -> Result<CostSummary, String>;

    /// Get daily cost trend for the last 30 days
    fn get_cost_trend(&self, start_date: &str, end_date: &str)
        -> Result<CostTrend, String>;
}
```

### Step 3: Create Provider File

```bash
# Create new provider file
touch src/cloud/azure.rs

# Or for GCP
touch src/cloud/gcp.rs
```

### Step 4: Implement the Provider

#### Template Structure

```rust
// src/cloud/azure.rs
use crate::cloud::{CloudService, CostData, CostSummary, CostTrend};
use anyhow::{Result, Context};
use serde::{Deserialize, Serialize};

pub struct AzureClient {
    subscription_id: String,
    tenant_id: String,
    client_id: String,
    client_secret: String,
}

impl AzureClient {
    pub fn new(subscription_id: String, tenant_id: String,
               client_id: String, client_secret: String) -> Self {
        Self {
            subscription_id,
            tenant_id,
            client_id,
            client_secret,
        }
    }

    /// Get OAuth token for Azure API
    fn get_access_token(&self) -> Result<String> {
        let token_url = format!(
            "https://login.microsoftonline.com/{}/oauth2/v2.0/token",
            self.tenant_id
        );

        let response = ureq::post(&token_url)
            .send_form(&[
                ("grant_type", "client_credentials"),
                ("client_id", &self.client_id),
                ("client_secret", &self.client_secret),
                ("scope", "https://management.azure.com/.default"),
            ])
            .context("Failed to get Azure access token")?;

        let body: serde_json::Value = response.into_json()?;
        let token = body["access_token"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("No access token in response"))?
            .to_string();

        Ok(token)
    }

    /// Make authenticated request to Azure Cost Management API
    fn make_cost_request(&self, endpoint: &str, body: &str) -> Result<String> {
        let token = self.get_access_token()?;
        let url = format!(
            "https://management.azure.com/subscriptions/{}/providers/Microsoft.CostManagement/{}",
            self.subscription_id, endpoint
        );

        let response = ureq::post(&url)
            .set("Authorization", &format!("Bearer {}", token))
            .set("Content-Type", "application/json")
            .send_string(body)
            .context("Failed to call Azure Cost Management API")?;

        let body = response.into_string()?;
        Ok(body)
    }
}

impl CloudService for AzureClient {
    fn validate_credentials(&self) -> Result<bool, String> {
        // Try to get an access token
        match self.get_access_token() {
            Ok(_) => Ok(true),
            Err(e) => Err(format!("Azure credentials validation failed: {}", e)),
        }
    }

    fn get_cost_data(&self, start_date: &str, end_date: &str)
        -> Result<Vec<CostData>, String> {
        // Build Azure Cost Management query
        let query = serde_json::json!({
            "type": "Usage",
            "timeframe": "Custom",
            "timePeriod": {
                "from": start_date,
                "to": end_date
            },
            "dataset": {
                "granularity": "Daily",
                "aggregation": {
                    "totalCost": {
                        "name": "Cost",
                        "function": "Sum"
                    }
                },
                "grouping": [
                    {
                        "type": "Dimension",
                        "name": "ServiceName"
                    }
                ]
            }
        });

        let response = self.make_cost_request(
            "query?api-version=2023-11-01",
            &query.to_string()
        ).map_err(|e| e.to_string())?;

        // Parse response
        self.parse_cost_data(&response)
    }

    fn get_cost_summary(&self) -> Result<CostSummary, String> {
        // Get current month costs
        let now = chrono::Utc::now();
        let start = now.format("%Y-%m-01").to_string();
        let end = now.format("%Y-%m-%d").to_string();

        let cost_data = self.get_cost_data(&start, &end)?;

        // Aggregate by service
        let mut services = std::collections::HashMap::new();
        let mut total = 0.0;

        for data in cost_data {
            *services.entry(data.service_name.clone()).or_insert(0.0) += data.cost;
            total += data.cost;
        }

        let service_costs = services.into_iter()
            .map(|(name, cost)| crate::cloud::ServiceCost {
                service_name: name,
                cost
            })
            .collect();

        Ok(CostSummary {
            total_cost: total,
            currency: "USD".to_string(),
            service_costs,
            start_date: start,
            end_date: end,
        })
    }

    fn get_cost_trend(&self, start_date: &str, end_date: &str)
        -> Result<CostTrend, String> {
        let cost_data = self.get_cost_data(start_date, end_date)?;

        // Group by date
        let mut daily_costs = std::collections::HashMap::new();
        for data in cost_data {
            *daily_costs.entry(data.date.clone()).or_insert(0.0) += data.cost;
        }

        let mut dates: Vec<_> = daily_costs.keys().cloned().collect();
        dates.sort();

        let costs: Vec<f64> = dates.iter()
            .map(|date| daily_costs[date])
            .collect();

        Ok(CostTrend {
            dates,
            costs,
            currency: "USD".to_string(),
        })
    }
}

impl AzureClient {
    /// Parse Azure Cost Management API response
    fn parse_cost_data(&self, json: &str) -> Result<Vec<CostData>, String> {
        #[derive(Deserialize)]
        struct AzureResponse {
            properties: Properties,
        }

        #[derive(Deserialize)]
        struct Properties {
            rows: Vec<Vec<serde_json::Value>>,
            columns: Vec<Column>,
        }

        #[derive(Deserialize)]
        struct Column {
            name: String,
        }

        let response: AzureResponse = serde_json::from_str(json)
            .map_err(|e| format!("Failed to parse Azure response: {}", e))?;

        let mut cost_data = Vec::new();

        for row in response.properties.rows {
            let cost = row[0].as_f64().unwrap_or(0.0);
            let date = row[1].as_str().unwrap_or("").to_string();
            let service_name = row[2].as_str().unwrap_or("Unknown").to_string();

            cost_data.push(CostData {
                date,
                service_name,
                cost,
                currency: "USD".to_string(),
            });
        }

        Ok(cost_data)
    }
}
```

### Step 5: Register Provider in mod.rs

```rust
// src/cloud/mod.rs
pub mod aws;
pub mod aliyun;
pub mod deepseek;
pub mod azure;  // Add your new provider

pub use aws::AwsClient;
pub use aliyun::AliyunClient;
pub use deepseek::DeepSeekClient;
pub use azure::AzureClient;  // Export new provider
```

### Step 6: Add to Provider Enum

```rust
// src/cloud/mod.rs
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CloudProvider {
    AWS,
    Aliyun,
    DeepSeek,
    Azure,  // Add new provider
    GCP,
}

impl CloudProvider {
    pub fn name(&self) -> &str {
        match self {
            CloudProvider::AWS => "AWS",
            CloudProvider::Aliyun => "Alibaba Cloud",
            CloudProvider::DeepSeek => "DeepSeek",
            CloudProvider::Azure => "Microsoft Azure",  // Add name
            CloudProvider::GCP => "Google Cloud",
        }
    }

    pub fn all() -> Vec<CloudProvider> {
        vec![
            CloudProvider::AWS,
            CloudProvider::Aliyun,
            CloudProvider::DeepSeek,
            CloudProvider::Azure,  // Add to list
            CloudProvider::GCP,
        ]
    }
}
```

### Step 7: Update Database Schema

```rust
// src/db.rs - Update CloudAccount struct if needed
pub struct CloudAccount {
    pub id: String,
    pub name: String,
    pub provider: String,  // "aws", "aliyun", "azure", etc.
    pub access_key: String,  // Encrypted
    pub secret_key: String,  // Encrypted
    pub region: Option<String>,
    // Add provider-specific fields as JSON
    pub extra_config: Option<String>,  // JSON for provider-specific config
}
```

### Step 8: Update UI (Account Management)

```rust
// src/ui/accounts.rs

// Add provider to dropdown
fn render_provider_selector(&self, cx: &ViewContext<Self>) -> impl IntoElement {
    div()
        .children(CloudProvider::all().into_iter().map(|provider| {
            Button::new(provider.name())
                .on_click(cx.listener(move |this, _, cx| {
                    this.selected_provider = provider.clone();
                    cx.notify();
                }))
        }))
}

// Add provider-specific form fields
fn render_credential_form(&self, cx: &ViewContext<Self>) -> impl IntoElement {
    match self.selected_provider {
        CloudProvider::Azure => self.render_azure_form(cx),
        CloudProvider::AWS => self.render_aws_form(cx),
        // ... other providers
    }
}

fn render_azure_form(&self, cx: &ViewContext<Self>) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap_4()
        .child(self.render_input("Subscription ID", &self.subscription_id_input))
        .child(self.render_input("Tenant ID", &self.tenant_id_input))
        .child(self.render_input("Client ID", &self.client_id_input))
        .child(self.render_input("Client Secret", &self.client_secret_input))
}
```

### Step 9: Add Tests

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_azure_client_creation() {
        let client = AzureClient::new(
            "sub-id".to_string(),
            "tenant-id".to_string(),
            "client-id".to_string(),
            "secret".to_string(),
        );

        assert_eq!(client.subscription_id, "sub-id");
    }

    #[test]
    fn test_parse_azure_cost_response() {
        let json = r#"{
            "properties": {
                "rows": [[100.50, 20240101000000, "Virtual Machines"]],
                "columns": [
                    {"name": "Cost"},
                    {"name": "Date"},
                    {"name": "ServiceName"}
                ]
            }
        }"#;

        let client = AzureClient::new(
            "sub".to_string(),
            "tenant".to_string(),
            "client".to_string(),
            "secret".to_string(),
        );

        let result = client.parse_cost_data(json).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].cost, 100.50);
        assert_eq!(result[0].service_name, "Virtual Machines");
    }
}
```

### Step 10: Update Documentation

```rust
// Update README.md with new provider setup instructions
// Update CHANGELOG.md with new feature
// Add provider-specific docs to docs/ directory
```

## Provider Implementation Checklist

- [ ] Create new file in `src/cloud/`
- [ ] Implement `CloudService` trait
- [ ] Add authentication logic
- [ ] Implement `validate_credentials()`
- [ ] Implement `get_cost_data()`
- [ ] Implement `get_cost_summary()`
- [ ] Implement `get_cost_trend()`
- [ ] Add response parsing logic
- [ ] Register in `src/cloud/mod.rs`
- [ ] Add to `CloudProvider` enum
- [ ] Update UI forms in `src/ui/accounts.rs`
- [ ] Update database schema if needed
- [ ] Add unit tests
- [ ] Add integration tests (with mocked API)
- [ ] Test credential validation
- [ ] Test cost data retrieval
- [ ] Update documentation
- [ ] Update README.md

## Common Patterns

### Pattern 1: OAuth Authentication (Azure, GCP)

```rust
fn get_access_token(&self) -> Result<String> {
    let response = ureq::post(&token_url)
        .send_form(&[
            ("grant_type", "client_credentials"),
            ("client_id", &self.client_id),
            ("client_secret", &self.client_secret),
            ("scope", &self.scope),
        ])?;

    let body: serde_json::Value = response.into_json()?;
    Ok(body["access_token"].as_str().unwrap().to_string())
}
```

### Pattern 2: Signature-based Auth (AWS, Alibaba)

```rust
fn sign_request(&self, request: &Request) -> String {
    let string_to_sign = format!(
        "{}\n{}\n{}\n{}",
        request.method,
        request.path,
        request.date,
        request.params
    );

    let signature = hmac_sha256(&self.secret_key, &string_to_sign);
    base64::encode(signature)
}
```

### Pattern 3: API Key Auth (Simple APIs)

```rust
fn make_request(&self, endpoint: &str) -> Result<String> {
    let response = ureq::get(endpoint)
        .set("Authorization", &format!("Bearer {}", self.api_key))
        .call()?;

    Ok(response.into_string()?)
}
```

## Testing with Mock APIs

```rust
#[cfg(test)]
mod tests {
    use mockito::{Server, Mock};

    #[test]
    fn test_azure_cost_api() {
        let mut server = Server::new();

        let mock = server.mock("POST", "/query")
            .with_status(200)
            .with_body(r#"{"properties": {"rows": [], "columns": []}}"#)
            .create();

        let client = AzureClient::new_with_endpoint(
            &server.url(),
            "sub".to_string(),
            "tenant".to_string(),
            "client".to_string(),
            "secret".to_string(),
        );

        let result = client.get_cost_data("2024-01-01", "2024-01-31");
        assert!(result.is_ok());
        mock.assert();
    }
}
```

## Provider-Specific Considerations

### Azure
- OAuth 2.0 token-based authentication
- Cost Management API requires specific permissions
- Subscription ID is the main identifier
- Multi-tenant scenarios need careful handling

### Google Cloud Platform
- OAuth 2.0 with service accounts
- Cloud Billing API
- Project ID is the main identifier
- IAM roles: `roles/billing.viewer`

### Oracle Cloud
- Signature-based authentication
- Cost and Usage Reports API
- Tenancy OCID required
- Region-specific endpoints

### DigitalOcean
- API token authentication
- Billing API endpoint
- Simple REST API
- Monthly invoice data

## Resources

- Azure Cost Management API: https://learn.microsoft.com/en-us/rest/api/cost-management/
- GCP Cloud Billing API: https://cloud.google.com/billing/docs/reference/rest
- AWS Cost Explorer API: https://docs.aws.amazon.com/aws-cost-management/latest/APIReference/
- CloudBridge Examples: `src/cloud/` directory
