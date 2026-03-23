# Reduce Clones Skill

This skill helps identify and eliminate unnecessary `.clone()` calls to optimize memory usage and performance in CloudBridge.

## When to Use

Use this skill when:
- Performance profiling shows excessive memory allocation
- Code review identifies unnecessary clones
- Refactoring to improve efficiency
- Adding new features and want to avoid clone overhead

## Why Clones Are Problematic

1. **Memory Overhead** - Each clone allocates new memory
2. **Performance Impact** - Cloning large structures is expensive
3. **Cache Pollution** - More allocations = worse cache performance
4. **Unnecessary Copies** - Often data can be borrowed instead

## Common Clone Patterns in CloudBridge

### Pattern 1: Cloning for View Passing

**Problem:**
```rust
// src/ui/dashboard.rs
impl Render for DashboardView {
    fn render(&mut self, cx: &mut ViewContext<Self>) -> impl IntoElement {
        div()
            .children(self.accounts.iter().map(|account| {
                // Cloning the entire account for each render
                self.render_account_card(account.clone(), cx)
            }))
    }

    fn render_account_card(&self, account: CloudAccount, cx: &ViewContext<Self>) -> impl IntoElement {
        div().child(&account.name)
    }
}
```

**Solution 1: Use References**
```rust
impl Render for DashboardView {
    fn render(&mut self, cx: &mut ViewContext<Self>) -> impl IntoElement {
        div()
            .children(self.accounts.iter().map(|account| {
                // Pass reference instead of clone
                self.render_account_card(account, cx)
            }))
    }

    fn render_account_card(&self, account: &CloudAccount, cx: &ViewContext<Self>) -> impl IntoElement {
        div().child(&account.name)
    }
}
```

**Solution 2: Use Arc for Shared Ownership**
```rust
use std::sync::Arc;

pub struct DashboardView {
    // Store accounts in Arc for cheap cloning
    accounts: Vec<Arc<CloudAccount>>,
}

impl Render for DashboardView {
    fn render(&mut self, cx: &mut ViewContext<Self>) -> impl IntoElement {
        div()
            .children(self.accounts.iter().map(|account| {
                // Arc::clone is just incrementing reference count
                self.render_account_card(Arc::clone(account), cx)
            }))
    }

    fn render_account_card(&self, account: Arc<CloudAccount>, cx: &ViewContext<Self>) -> impl IntoElement {
        div().child(&account.name)
    }
}
```

### Pattern 2: Cloning Strings

**Problem:**
```rust
// Cloning strings unnecessarily
fn format_cost(amount: f64, currency: String) -> String {
    format!("{:.2} {}", amount, currency)  // currency moved here
}

let currency = "USD".to_string();
let formatted1 = format_cost(100.0, currency.clone());  // Clone!
let formatted2 = format_cost(200.0, currency.clone());  // Clone!
```

**Solution 1: Use String Slices**
```rust
fn format_cost(amount: f64, currency: &str) -> String {
    format!("{:.2} {}", amount, currency)
}

let currency = "USD";  // &str instead of String
let formatted1 = format_cost(100.0, currency);  // No clone
let formatted2 = format_cost(200.0, currency);  // No clone
```

**Solution 2: Use Owned When Necessary**
```rust
fn format_cost(amount: f64, currency: impl AsRef<str>) -> String {
    format!("{:.2} {}", amount, currency.as_ref())
}

// Works with both &str and String
let formatted1 = format_cost(100.0, "USD");
let formatted2 = format_cost(200.0, String::from("EUR"));
```

### Pattern 3: Cloning in Closures

**Problem:**
```rust
let account_name = account.name.clone();
let account_id = account.id.clone();

Button::new("delete")
    .on_click(cx.listener(move |this, _event, cx| {
        // Closure captures clones
        this.delete_account(&account_name, &account_id, cx);
    }))
```

**Solution 1: Capture References (if lifetime allows)**
```rust
// Store account in a way that outlives the closure
let account_ref = &self.accounts[index];

Button::new("delete")
    .on_click(cx.listener(move |this, _event, cx| {
        this.delete_account(&account_ref.name, &account_ref.id, cx);
    }))
```

**Solution 2: Clone Only What's Needed**
```rust
// Clone only the small pieces
let account_id = account.id;  // Copy for simple types

Button::new("delete")
    .on_click(cx.listener(move |this, _event, cx| {
        // Only account_id is captured, no string clones
        this.delete_account(account_id, cx);
    }))
```

**Solution 3: Use Arc**
```rust
let account = Arc::clone(&self.accounts[index]);

Button::new("delete")
    .on_click(cx.listener(move |this, _event, cx| {
        // Arc clone is cheap (just a reference count increment)
        this.delete_account(Arc::clone(&account), cx);
    }))
```

### Pattern 4: Cloning in Iterations

**Problem:**
```rust
// Cloning in filter_map
let active_accounts: Vec<CloudAccount> = self.accounts.clone()
    .into_iter()
    .filter(|a| a.is_active)
    .collect();
```

**Solution:**
```rust
// Use references
let active_accounts: Vec<&CloudAccount> = self.accounts
    .iter()
    .filter(|a| a.is_active)
    .collect();

// Or if you need owned values and can't use references:
let active_accounts: Vec<CloudAccount> = self.accounts
    .iter()
    .filter(|a| a.is_active)
    .cloned()  // Clone only filtered items, not all items first
    .collect();
```

### Pattern 5: Cloning in Async Contexts

**Problem:**
```rust
let accounts = self.accounts.clone();
let config = self.config.clone();

cx.spawn(move |this, mut cx| async move {
    // Using cloned data in async context
    let results = fetch_costs(&accounts, &config).await;
    // ...
}).detach();
```

**Solution 1: Use Arc**
```rust
use std::sync::Arc;

pub struct DashboardView {
    accounts: Arc<Vec<CloudAccount>>,
    config: Arc<Config>,
}

// In async spawn
let accounts = Arc::clone(&self.accounts);
let config = Arc::clone(&self.config);

cx.spawn(move |this, mut cx| async move {
    let results = fetch_costs(&accounts, &config).await;
    // ...
}).detach();
```

**Solution 2: Clone Only IDs**
```rust
// Instead of cloning entire accounts
let account_ids: Vec<String> = self.accounts
    .iter()
    .map(|a| a.id.clone())
    .collect();

cx.spawn(move |this, mut cx| async move {
    // Fetch using just IDs
    let results = fetch_costs_by_ids(&account_ids).await;
    // ...
}).detach();
```

## Refactoring Strategy

### Step 1: Find Clones

```bash
# Search for .clone() calls
rg "\.clone\(\)" --type rust

# Count clones per file
rg "\.clone\(\)" --type rust --count

# Find expensive clones (structs, vecs)
rg "Vec<.*>.*\.clone\(\)" --type rust
rg "HashMap<.*>.*\.clone\(\)" --type rust
```

### Step 2: Analyze Each Clone

For each `.clone()`, ask:

1. **Is it necessary?**
   - Can we use a reference instead?
   - Can we restructure to avoid the need?

2. **Is it expensive?**
   - Simple types (i32, bool) - Copy is fine
   - Strings, Vecs - Consider alternatives
   - Large structs - Definitely avoid if possible

3. **What's the alternative?**
   - Use `&T` reference
   - Use `Arc<T>` for shared ownership
   - Use `Cow<T>` for clone-on-write
   - Restructure code to avoid need

### Step 3: Replace Clones

#### Replacement Pattern 1: Function Signatures

```rust
// Before
fn process_account(account: CloudAccount) { }
let account = get_account();
process_account(account.clone());  // Clone needed because function takes ownership

// After
fn process_account(account: &CloudAccount) { }
let account = get_account();
process_account(&account);  // No clone, just borrow
```

#### Replacement Pattern 2: Struct Fields

```rust
// Before
pub struct View {
    data: Vec<Item>,  // Owned
}

fn render(&self) -> Element {
    // Need to clone to pass to helper
    self.render_list(self.data.clone())
}

// After
pub struct View {
    data: Arc<Vec<Item>>,  // Shared
}

fn render(&self) -> Element {
    // Arc clone is cheap
    self.render_list(Arc::clone(&self.data))
}
```

#### Replacement Pattern 3: Return Values

```rust
// Before
fn get_accounts(&self) -> Vec<CloudAccount> {
    self.accounts.clone()  // Clone entire vector
}

// After - Option 1: Return reference
fn get_accounts(&self) -> &[CloudAccount] {
    &self.accounts
}

// After - Option 2: Return iterator
fn get_accounts(&self) -> impl Iterator<Item = &CloudAccount> {
    self.accounts.iter()
}
```

## Smart Clone Usage

Sometimes clones ARE necessary. Use them wisely:

### When Clone is Acceptable

1. **Simple Copy Types**
```rust
let count = other.count;  // i32 - Copy, not Clone
let flag = other.flag;    // bool - Copy, not Clone
```

2. **Small Strings**
```rust
// Short error messages - clone is fine
let error = "Invalid input".to_string();
```

3. **Breaking Borrow Checker Deadlocks**
```rust
// Sometimes clone is simplest solution to complex borrows
let temp = complex_data.clone();
self.process(&temp);  // Avoids borrow checker issues
```

4. **Async Boundaries**
```rust
// Need owned data in async block
let name = self.name.clone();
tokio::spawn(async move {
    process(&name).await
});
```

### Clone Alternatives Cheat Sheet

| Scenario | Instead of Clone | Use |
|----------|-----------------|-----|
| Read-only access | `data.clone()` | `&data` |
| Shared ownership | `data.clone()` | `Arc<T>` |
| Maybe modify | `data.clone()` | `Cow<T>` |
| Async context | `data.clone()` | `Arc<T>` |
| Thread send | `data.clone()` | `Arc<T>` |
| Function param | `fn(data: T)` | `fn(data: &T)` |
| Return value | `return data.clone()` | `return &data` or `Arc<T>` |

## Measuring Impact

### Before Refactoring
```bash
# Run with memory profiling
cargo build --release
valgrind --tool=massif ./target/release/cloudbridge

# Or use built-in allocator stats
cargo run --release -- --mem-stats
```

### After Refactoring
```bash
# Compare memory usage
# Should see:
# - Fewer allocations
# - Lower peak memory
# - Faster execution
```

## CloudBridge Specific Patterns

### Pattern: Dashboard Account Cards

**Before (with clones):**
```rust
// In dashboard.rs
self.accounts.iter().map(|account| {
    div().child(self.render_account_card(account.clone()))
})
```

**After (without clones):**
```rust
self.accounts.iter().map(|account| {
    div().child(self.render_account_card(account))
})

fn render_account_card(&self, account: &CloudAccount) -> impl IntoElement {
    // Use reference throughout
}
```

### Pattern: Cloud Provider Clients

**Before:**
```rust
pub struct AwsClient {
    access_key: String,
    secret_key: String,
    region: String,
}

// Expensive clones for signing
fn sign_request(&self, req: &Request) -> SignedRequest {
    let ak = self.access_key.clone();
    let sk = self.secret_key.clone();
    let region = self.region.clone();
    // ... signing logic
}
```

**After:**
```rust
pub struct AwsClient {
    access_key: Arc<str>,  // Use Arc for immutable shared strings
    secret_key: Arc<str>,
    region: Arc<str>,
}

fn sign_request(&self, req: &Request) -> SignedRequest {
    // Just pass references - no clones
    sign_with_v4(&self.access_key, &self.secret_key, &self.region, req)
}
```

## Checklist

- [ ] Search for all `.clone()` calls
- [ ] Categorize by necessity (required vs unnecessary)
- [ ] Identify expensive clones (large structs, collections)
- [ ] Replace with references where possible
- [ ] Use `Arc<T>` for shared ownership
- [ ] Update function signatures to accept references
- [ ] Update struct fields to use `Arc<T>` if needed
- [ ] Test performance before/after
- [ ] Verify memory usage improvement
- [ ] Document any remaining necessary clones

## Resources

- [The Rust Book - References and Borrowing](https://doc.rust-lang.org/book/ch04-02-references-and-borrowing.html)
- [Rust Performance Book - Clone](https://nnethercote.github.io/perf-book/clone.html)
- [Arc Documentation](https://doc.rust-lang.org/std/sync/struct.Arc.html)
