# Refactor Large Files Skill

This skill helps identify and refactor large files in the CloudBridge codebase to improve maintainability.

## When to Use

Use this skill when:
- A file exceeds 500 lines of code
- A component has multiple responsibilities
- Code review suggests splitting files
- Adding new features to already large files

## Large Files in CloudBridge

Current large files (as of analysis):
- `src/ui/dashboard.rs` - 837 lines
- `src/ui/accounts.rs` - 684 lines
- `src/cloud/aws.rs` - 758 lines
- `src/ui/chart.rs` - 545 lines
- `src/cloud/aliyun.rs` - 480 lines

## Refactoring Patterns

### Pattern 1: Extract Subcomponents

**Before: dashboard.rs (837 lines)**
```rust
// All in one file
impl DashboardView {
    fn render(&mut self, cx: &mut ViewContext<Self>) -> impl IntoElement {
        div()
            .child(self.render_overview_cards())
            .child(self.render_account_cards())
            .child(self.render_trend_modal())
    }

    fn render_overview_cards(&self, cx: &ViewContext<Self>) -> impl IntoElement {
        // 100+ lines of code
    }

    fn render_account_cards(&self, cx: &ViewContext<Self>) -> impl IntoElement {
        // 200+ lines of code
    }

    fn render_trend_modal(&self, cx: &ViewContext<Self>) -> impl IntoElement {
        // 150+ lines of code
    }
}
```

**After: Split into multiple files**

```
src/ui/dashboard/
├── mod.rs              # Main dashboard view (200 lines)
├── overview_cards.rs   # Overview statistics (150 lines)
├── account_card.rs     # Individual account card (200 lines)
└── trend_modal.rs      # Trend chart modal (250 lines)
```

**Implementation:**

```rust
// src/ui/dashboard/mod.rs
mod overview_cards;
mod account_card;
mod trend_modal;

use overview_cards::OverviewCards;
use account_card::AccountCard;
use trend_modal::TrendModal;

pub struct DashboardView {
    overview: View<OverviewCards>,
    accounts: Vec<View<AccountCard>>,
    trend_modal: Option<View<TrendModal>>,
}

impl Render for DashboardView {
    fn render(&mut self, cx: &mut ViewContext<Self>) -> impl IntoElement {
        div()
            .child(self.overview.clone())
            .children(self.accounts.clone())
            .children(self.trend_modal.clone())
    }
}

// src/ui/dashboard/overview_cards.rs
pub struct OverviewCards {
    current_month_cost: f64,
    last_month_cost: f64,
    account_count: usize,
}

impl Render for OverviewCards {
    fn render(&mut self, cx: &mut ViewContext<Self>) -> impl IntoElement {
        div()
            .flex()
            .gap_4()
            .child(self.render_cost_card("This Month", self.current_month_cost))
            .child(self.render_cost_card("Last Month", self.last_month_cost))
            .child(self.render_count_card("Accounts", self.account_count))
    }
}
```

### Pattern 2: Extract Data Logic

**Before: aws.rs (758 lines with mixed concerns)**

```rust
// aws.rs - Both HTTP logic and data parsing
impl AwsClient {
    fn get_cost_data(&self) -> Result<Vec<CostData>> {
        // 50 lines of AWS Signature V4 calculation
        // 30 lines of HTTP request building
        // 40 lines of response parsing
        // 50 lines of error handling
    }
}
```

**After: Separate concerns**

```
src/cloud/aws/
├── mod.rs          # Public API and client struct
├── auth.rs         # AWS Signature V4 signing
├── api.rs          # HTTP request/response handling
├── parser.rs       # JSON response parsing
└── types.rs        # AWS-specific types
```

```rust
// src/cloud/aws/mod.rs
mod auth;
mod api;
mod parser;
mod types;

pub use types::*;

pub struct AwsClient {
    credentials: AwsCredentials,
}

impl CloudService for AwsClient {
    fn get_cost_data(&self, start: &str, end: &str) -> Result<Vec<CostData>> {
        let request = api::build_cost_explorer_request(start, end)?;
        let signed = auth::sign_request(&request, &self.credentials)?;
        let response = api::send_request(&signed)?;
        parser::parse_cost_data(&response)
    }
}

// src/cloud/aws/auth.rs
pub fn sign_request(request: &Request, credentials: &AwsCredentials) -> Result<SignedRequest> {
    // AWS Signature V4 implementation
}

// src/cloud/aws/parser.rs
pub fn parse_cost_data(json: &str) -> Result<Vec<CostData>> {
    // JSON parsing logic
}
```

### Pattern 3: Extract State Management

**Before: accounts.rs (684 lines)**

```rust
pub struct AccountsView {
    // UI state
    selected_provider: String,
    name_input: String,
    ak_input: String,
    sk_input: String,
    region_input: String,
    validating: bool,
    error: Option<String>,

    // Data state
    accounts: Vec<CloudAccount>,

    // UI components
    focus_handle: FocusHandle,
}
```

**After: Separate state and view**

```rust
// src/ui/accounts/state.rs
pub struct AccountFormState {
    pub selected_provider: String,
    pub name_input: String,
    pub ak_input: String,
    pub sk_input: String,
    pub region_input: String,
    pub validating: bool,
    pub error: Option<String>,
}

impl AccountFormState {
    pub fn validate(&self) -> Result<(), String> {
        if self.name_input.is_empty() {
            return Err("Name is required".to_string());
        }
        // ... more validation
        Ok(())
    }

    pub fn reset(&mut self) {
        self.name_input.clear();
        self.ak_input.clear();
        self.sk_input.clear();
        self.error = None;
    }
}

// src/ui/accounts/mod.rs
mod state;
mod form;
mod list;

use state::AccountFormState;

pub struct AccountsView {
    form_state: Model<AccountFormState>,
    accounts: Vec<CloudAccount>,
    focus_handle: FocusHandle,
}
```

## Step-by-Step Refactoring Process

### 1. Analyze the File

```bash
# Count lines in large files
wc -l src/ui/dashboard.rs
wc -l src/ui/accounts.rs

# Identify logical sections
# Look for:
# - Multiple impl blocks
# - Groups of related methods
# - Large nested functions
# - Repeated patterns
```

### 2. Identify Boundaries

Look for natural separation points:
- **UI Components**: Different visual sections
- **Data Operations**: CRUD operations, queries
- **Business Logic**: Validation, calculations
- **External I/O**: HTTP requests, file operations

### 3. Create Module Structure

```bash
# For dashboard.rs
mkdir -p src/ui/dashboard
mv src/ui/dashboard.rs src/ui/dashboard/mod.rs

# Create submodule files
touch src/ui/dashboard/overview.rs
touch src/ui/dashboard/accounts.rs
touch src/ui/dashboard/trends.rs
```

### 4. Extract and Move Code

```rust
// Step 1: Copy the section to new file
// Step 2: Update imports in new file
// Step 3: Make struct/functions public
// Step 4: Import in mod.rs
// Step 5: Update references in original file
// Step 6: Test compilation
```

### 5. Update Tests

```rust
// Update test imports
#[cfg(test)]
mod tests {
    use super::*;
    // May need to update paths
    use crate::ui::dashboard::overview::OverviewCards;
}
```

## Common Refactoring Scenarios

### Scenario 1: Large UI Component

**Signs:**
- Render method > 100 lines
- Multiple helper render methods
- Complex state management

**Solution:**
```rust
// Extract visual sections into separate View structs
// Use composition in main view
// Share state via Model<T>
```

### Scenario 2: God Object (Many Responsibilities)

**Signs:**
- Struct with 10+ fields
- 20+ methods
- Multiple unrelated operations

**Solution:**
```rust
// Split into multiple focused structs
// Use traits for common behavior
// Inject dependencies
```

### Scenario 3: Mixed Concerns

**Signs:**
- UI code mixed with business logic
- HTTP code mixed with parsing
- Database code mixed with validation

**Solution:**
```rust
// Separate layers:
// - Presentation layer (UI)
// - Business logic layer (validation, rules)
// - Data access layer (API, DB)
```

## Refactoring Checklist

- [ ] File exceeds 500 lines
- [ ] Identify logical boundaries
- [ ] Create new module structure
- [ ] Extract first section
- [ ] Update imports
- [ ] Compile and fix errors
- [ ] Run tests
- [ ] Extract next section
- [ ] Repeat until complete
- [ ] Remove old file if fully migrated
- [ ] Update documentation
- [ ] Commit changes

## Safety Rules

1. **One refactoring at a time** - Don't mix with feature changes
2. **Keep tests passing** - Compile after each extraction
3. **Maintain public API** - Don't break external users
4. **Preserve behavior** - No logic changes during refactor
5. **Commit frequently** - Small, reversible commits

## Example: Refactoring dashboard.rs

```bash
# Current structure
src/ui/dashboard.rs (837 lines)

# Target structure
src/ui/dashboard/
├── mod.rs              # Main coordinator (150 lines)
├── overview.rs         # Overview cards (120 lines)
├── account_card.rs     # Account display (180 lines)
├── trend_modal.rs      # Chart modal (200 lines)
└── state.rs           # Shared state (100 lines)

# Commands
mkdir -p src/ui/dashboard
git mv src/ui/dashboard.rs src/ui/dashboard/mod.rs

# Extract each section
# 1. Extract state to state.rs
# 2. Extract overview cards to overview.rs
# 3. Extract account card to account_card.rs
# 4. Extract trend modal to trend_modal.rs
# 5. Update mod.rs to use new modules

cargo build  # Verify after each extraction
cargo test   # Run tests after each step
```

## Benefits of Refactoring

1. **Easier to understand** - Smaller, focused files
2. **Easier to test** - Test individual components
3. **Easier to modify** - Clear boundaries
4. **Better reusability** - Extract common code
5. **Team collaboration** - Fewer merge conflicts

## When NOT to Refactor

- File is < 300 lines and cohesive
- Code is rarely changed
- No clear separation boundaries
- Under tight deadline (refactor later)
- Extensive changes planned anyway
