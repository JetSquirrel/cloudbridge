# Debug GPUI Issues Skill

This skill helps debug common GPUI framework issues in CloudBridge.

## When to Use

Use this skill when:
- UI not rendering correctly
- State updates not reflecting in UI
- Performance issues or lag
- Layout problems
- Event handlers not firing

## Common GPUI Issues

### Issue 1: State Update Not Rendering

**Symptom:** Changed state but UI doesn't update

**Cause:** Forgot to call `cx.notify()`

```rust
// BAD: State changed but UI not updated
impl MyView {
    fn update_count(&mut self, cx: &mut ViewContext<Self>) {
        self.count += 1;
        // Missing cx.notify()!
    }
}

// GOOD: Notify context after state change
impl MyView {
    fn update_count(&mut self, cx: &mut ViewContext<Self>) {
        self.count += 1;
        cx.notify();  // ✓ Triggers re-render
    }
}
```

**Solution:**
- Always call `cx.notify()` after changing state
- Or use `cx.update_model()` which notifies automatically

### Issue 2: Async Updates Not Working

**Symptom:** Data loaded but UI shows old data

**Cause:** Async context lost or not updated properly

```rust
// BAD: Direct mutation in async
cx.spawn(|this, mut cx| async move {
    let data = fetch_data().await;
    this.data = data;  // Error: can't mutate directly
}).detach();

// GOOD: Use cx.update
cx.spawn(|this, mut cx| async move {
    let data = fetch_data().await;
    this.update(&mut cx, |this, cx| {
        this.data = data;
        cx.notify();  // Important!
    }).ok();
}).detach();
```

**Solution:**
- Use `this.update(&mut cx, |this, cx| { ... })` in async
- Always call `cx.notify()` in the update closure
- Use `.ok()` to handle potential errors

### Issue 3: Thread Panics in Background Tasks

**Symptom:** App crashes or hangs after background operation

**Cause:** Unhandled panics in spawned threads

```rust
// BAD: Panic not handled
std::thread::spawn(move || {
    let result = risky_operation();  // Might panic
    tx.send(result).unwrap();  // Might panic if receiver dropped
});

// GOOD: Handle panics and errors
std::thread::spawn(move || {
    let result = std::panic::catch_unwind(|| {
        risky_operation()
    });

    match result {
        Ok(data) => {
            let _ = tx.send(Ok(data));  // Ignore send error
        }
        Err(_) => {
            let _ = tx.send(Err("Operation panicked".to_string()));
        }
    }
});
```

**Solution:**
- Use `catch_unwind` for panic recovery
- Use `let _ = tx.send()` to ignore send errors
- Return `Result` types from background tasks

### Issue 4: Entity/View Lifecycle Issues

**Symptom:** "Entity not found" or "View dropped" errors

**Cause:** Trying to update dropped view or entity

```rust
// BAD: May reference dropped entity
let entity = self.my_entity.clone();
cx.spawn(|this, mut cx| async move {
    sleep(Duration::from_secs(5)).await;
    entity.update(&mut cx, |entity, cx| {
        // Entity might be dropped by now!
        entity.value = 42;
    });
}).detach();

// GOOD: Check if entity still exists
let entity = self.my_entity.clone();
cx.spawn(|this, mut cx| async move {
    sleep(Duration::from_secs(5)).await;
    if let Ok(_) = entity.update(&mut cx, |entity, cx| {
        entity.value = 42;
        cx.notify();
    }) {
        // Entity still exists
    }
}).detach();
```

**Solution:**
- Check update result with pattern matching
- Use weak references if appropriate
- Clean up async tasks when view is dropped

### Issue 5: Layout Not Working as Expected

**Symptom:** Elements overlapping, wrong sizes, or positions

**Cause:** Incorrect flexbox setup

```rust
// BAD: Conflicting layout properties
div()
    .flex()  // Flexbox
    .absolute()  // Absolute positioning (conflicts!)
    .child(...)

// BAD: Missing flex_1 for child
div()
    .flex()
    .flex_col()
    .child(
        div().child("Header")  // No flex properties
    )
    .child(
        div().child("Content")  // Should grow to fill
    )

// GOOD: Proper flex layout
div()
    .flex()
    .flex_col()
    .h_full()  // Fill height
    .child(
        div()
            .h(px(60.0))  // Fixed height header
            .child("Header")
    )
    .child(
        div()
            .flex_1()  // Grow to fill remaining space
            .child("Content")
    )
```

**Solution:**
- Use consistent layout mode (flex OR absolute)
- Use `flex_1()` for children that should grow
- Set explicit sizes or constraints
- Use `w_full()` and `h_full()` appropriately

### Issue 6: Event Handlers Not Firing

**Symptom:** Clicks or other events not working

**Cause:** Missing `cx.listener` or incorrect event setup

```rust
// BAD: Wrong closure type
Button::new("click-me")
    .on_click(|this, event, cx| {
        // This won't compile - wrong signature
        this.handle_click();
    })

// GOOD: Use cx.listener
Button::new("click-me")
    .on_click(cx.listener(|this, event, cx| {
        this.handle_click(cx);
    }))

// GOOD: For simple cases
Button::new("click-me")
    .on_click(cx.listener(|this, _event, cx| {
        this.count += 1;
        cx.notify();
    }))
```

**Solution:**
- Always wrap event handlers with `cx.listener()`
- Ensure proper closure signature
- Call `cx.notify()` if state changes

### Issue 7: Performance Issues / Lag

**Symptom:** UI feels slow or unresponsive

**Causes & Solutions:**

```rust
// CAUSE 1: Expensive computation in render
impl Render for MyView {
    fn render(&mut self, cx: &mut ViewContext<Self>) -> impl IntoElement {
        // BAD: Heavy computation every render
        let processed = self.items.iter()
            .map(|item| expensive_processing(item))
            .collect::<Vec<_>>();

        div().children(processed.into_iter().map(|p| div().child(p)))
    }
}

// SOLUTION: Cache computed values
pub struct MyView {
    items: Vec<Item>,
    cached_processed: Vec<ProcessedItem>,
}

impl MyView {
    fn update_items(&mut self, items: Vec<Item>, cx: &mut ViewContext<Self>) {
        self.items = items;
        // Compute once when data changes
        self.cached_processed = self.items.iter()
            .map(|item| expensive_processing(item))
            .collect();
        cx.notify();
    }
}

// CAUSE 2: Too many renders
impl MyView {
    fn on_timer(&mut self, cx: &mut ViewContext<Self>) {
        self.tick_count += 1;
        cx.notify();  // Renders entire view every tick!
    }
}

// SOLUTION: Update only what changed
// Use separate views for different update frequencies
pub struct MyView {
    static_content: View<StaticContent>,
    dynamic_ticker: View<Ticker>,
}

// CAUSE 3: Large lists without virtualization
div()
    .children(self.thousands_of_items.iter().map(|item| {
        div().child(render_item(item))
    }))

// SOLUTION: Implement virtual scrolling
// Or limit visible items
div()
    .children(self.items.iter()
        .take(100)  // Show only first 100
        .map(|item| div().child(render_item(item)))
    )
```

### Issue 8: Theme Colors Not Working

**Symptom:** Colors not applying or incorrect

```rust
// BAD: Accessing theme wrong way
.bg(self.theme.background)  // theme might not be accessible

// GOOD: Use cx.theme()
.bg(cx.theme().background)

// BAD: Custom color not in theme
.bg(rgb(0x1e1e1e))  // Hardcoded

// GOOD: Extend theme if needed
.bg(cx.theme().background)
```

**Solution:**
- Always use `cx.theme()` to access theme
- Add custom colors to theme system if needed
- Don't hardcode colors

### Issue 9: Memory Leaks

**Symptom:** Memory usage grows over time

**Causes:**
1. Not detaching spawned tasks
2. Circular references with Arc
3. Keeping old views alive

```rust
// BAD: Task not detached
cx.spawn(|this, mut cx| async move {
    // Long-running task
});  // Missing .detach()!

// GOOD: Detach background tasks
cx.spawn(|this, mut cx| async move {
    // Long-running task
}).detach();

// BAD: Circular reference
struct Parent {
    child: Arc<Child>,
}

struct Child {
    parent: Arc<Parent>,  // Circular!
}

// GOOD: Use weak references
struct Child {
    parent: Weak<Parent>,  // Weak reference breaks cycle
}
```

### Issue 10: Model/View Synchronization

**Symptom:** Model updates but view shows stale data

```rust
// BAD: Cloning model instead of referencing
pub struct MyView {
    data: MyData,  // Owned copy
}

// Model updates elsewhere but view has stale copy

// GOOD: Reference shared model
pub struct MyView {
    data: Model<MyData>,  // Shared reference
}

impl MyView {
    fn new(data: Model<MyData>, cx: &mut ViewContext<Self>) -> Self {
        // Subscribe to model changes
        cx.observe(&data, |this, model, cx| {
            // Called when model changes
            cx.notify();
        }).detach();

        Self { data }
    }
}
```

## Debugging Tools

### 1. Enable GPUI Logging

```rust
// In main.rs
env_logger::init();

// Set log level
std::env::set_var("RUST_LOG", "debug");
```

### 2. Add Debug Prints

```rust
impl Render for MyView {
    fn render(&mut self, cx: &mut ViewContext<Self>) -> impl IntoElement {
        eprintln!("Rendering MyView, count: {}", self.count);
        div().child(format!("Count: {}", self.count))
    }
}
```

### 3. Check View Bounds

```rust
div()
    .border_1()  // Add border to see actual size
    .border_color(rgb(0xff0000))  // Red border
    .child(content)
```

### 4. Inspect Context

```rust
// Check if view is still alive
if let Ok(_) = this.update(&mut cx, |_, _| {}) {
    println!("View is alive");
} else {
    println!("View is dropped!");
}
```

## Common Patterns for Debugging

### Pattern 1: State Change Debugging

```rust
impl MyView {
    fn set_value(&mut self, value: i32, cx: &mut ViewContext<Self>) {
        eprintln!("set_value called: {} -> {}", self.value, value);
        self.value = value;
        eprintln!("Calling cx.notify()");
        cx.notify();
        eprintln!("cx.notify() completed");
    }
}
```

### Pattern 2: Async Flow Debugging

```rust
cx.spawn(|this, mut cx| async move {
    eprintln!("Async task started");
    let result = fetch_data().await;
    eprintln!("Data fetched: {:?}", result);

    this.update(&mut cx, |this, cx| {
        eprintln!("Updating view with result");
        this.data = result;
        cx.notify();
        eprintln!("View updated");
    }).ok();

    eprintln!("Async task completed");
}).detach();
```

### Pattern 3: Event Flow Debugging

```rust
Button::new("test")
    .on_click(cx.listener(|this, event, cx| {
        eprintln!("Button clicked!");
        eprintln!("  Position: {:?}", event.position);
        eprintln!("  Current count: {}", this.count);
        this.count += 1;
        eprintln!("  New count: {}", this.count);
        cx.notify();
        eprintln!("  Notified context");
    }))
```

## Checklist

When debugging GPUI issues, check:

- [ ] Is `cx.notify()` called after state changes?
- [ ] Are async updates using `this.update(&mut cx, ...)`?
- [ ] Are spawned tasks `.detach()`ed?
- [ ] Are event handlers wrapped with `cx.listener()`?
- [ ] Is flex layout setup correctly?
- [ ] Are theme colors accessed via `cx.theme()`?
- [ ] Are expensive operations cached?
- [ ] Are model updates observed?
- [ ] Are circular references avoided?
- [ ] Are errors handled in background tasks?

## Resources

- GPUI Documentation: https://gpui.rs/
- GPUI Examples: https://github.com/zed-industries/zed
- CloudBridge Examples: `src/ui/` directory
