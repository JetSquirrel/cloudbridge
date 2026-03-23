# GPUI Style Guide Skill

This skill provides GPUI framework coding standards and best practices for the CloudBridge project.

## When to Use

Use this skill when:
- Writing new GPUI UI components
- Refactoring existing UI code
- Reviewing code for GPUI patterns
- Need guidance on GPUI best practices

## GPUI Coding Standards

### 1. Component Structure

```rust
use gpui::*;

pub struct MyView {
    // State first
    data: Model<MyData>,
    // UI state
    focus_handle: FocusHandle,
}

impl MyView {
    pub fn new(cx: &mut WindowContext) -> Self {
        Self {
            data: cx.new_model(|_| MyData::default()),
            focus_handle: cx.focus_handle(),
        }
    }
}

impl Render for MyView {
    fn render(&mut self, cx: &mut ViewContext<Self>) -> impl IntoElement {
        div()
            // Layout properties first
            .w_full()
            .h_full()
            // Flexbox properties
            .flex()
            .flex_col()
            // Spacing
            .gap_4()
            .p_4()
            // Visual properties
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            // Children last
            .child(self.render_header(cx))
            .child(self.render_content(cx))
    }
}
```

### 2. Styling Order

Always apply styles in this order:
1. Size/dimensions (`w_*`, `h_*`, `size_*`)
2. Layout mode (`flex`, `relative`, `absolute`)
3. Flex properties (`flex_col`, `items_center`, `justify_between`)
4. Spacing (`gap_*`, `p_*`, `m_*`)
5. Border (`border_*`, `rounded_*`)
6. Background (`bg`)
7. Text (`text_*`, `font_*`)
8. Interactive states (`hover`, `active`)
9. Children (`.child()`, `.children()`)

### 3. Theme System

Always use theme colors instead of hardcoded values:

```rust
// Good
.bg(cx.theme().background)
.text_color(cx.theme().foreground)
.border_color(cx.theme().border)

// Avoid
.bg(rgb(0x1e1e1e))
.text_color(rgb(0xffffff))
```

### 4. State Management

```rust
// For simple local state
struct MyView {
    counter: usize,
}

// For shared state across components
struct MyView {
    shared_data: Model<SharedData>,
}

// Always notify on state changes
impl MyView {
    fn increment(&mut self, cx: &mut ViewContext<Self>) {
        self.counter += 1;
        cx.notify(); // Trigger re-render
    }
}
```

### 5. Async Operations

```rust
// Pattern 1: Background thread with channel (for blocking ops)
let (tx, rx) = std::sync::mpsc::channel();
std::thread::spawn(move || {
    let result = blocking_operation();
    let _ = tx.send(result);
});

cx.spawn(|mut cx| async move {
    if let Ok(data) = rx.recv() {
        cx.update(|cx| {
            // Update UI with data
        }).ok();
    }
}).detach();

// Pattern 2: Async spawn (for async ops)
cx.spawn(|this, mut cx| async move {
    let result = async_operation().await;
    this.update(&mut cx, |this, cx| {
        this.data = result;
        cx.notify();
    }).ok();
}).detach();
```

### 6. Component Composition

```rust
// Break down large views into smaller methods
impl MyView {
    fn render_header(&self, cx: &ViewContext<Self>) -> impl IntoElement {
        div()
            .h(px(60.0))
            .w_full()
            .child("Header")
    }

    fn render_sidebar(&self, cx: &ViewContext<Self>) -> impl IntoElement {
        div()
            .w(px(220.0))
            .h_full()
            .child("Sidebar")
    }

    fn render_content(&self, cx: &ViewContext<Self>) -> impl IntoElement {
        div()
            .flex_1()
            .child("Content")
    }
}
```

### 7. Input Handling

```rust
// Use on_click for interactive elements
Button::new("submit")
    .label("Submit")
    .on_click(cx.listener(|this, _event, cx| {
        this.handle_submit(cx);
    }))

// Use on_mouse_down for custom interactions
div()
    .on_mouse_down(MouseButton::Left, cx.listener(|this, event, cx| {
        this.handle_click(event.position, cx);
    }))
```

### 8. Error Handling in UI

```rust
// Pattern: Use Option/Result in state
struct MyView {
    data: Option<Vec<Item>>,
    error: Option<String>,
}

impl Render for MyView {
    fn render(&mut self, cx: &mut ViewContext<Self>) -> impl IntoElement {
        div().child(
            if let Some(error) = &self.error {
                div()
                    .text_color(cx.theme().error)
                    .child(format!("Error: {}", error))
                    .into_any_element()
            } else if let Some(data) = &self.data {
                self.render_data(data, cx).into_any_element()
            } else {
                div().child("Loading...").into_any_element()
            }
        )
    }
}
```

### 9. Performance Tips

```rust
// 1. Avoid unnecessary clones - use references when possible
// Bad
.children(items.clone().into_iter().map(|item| {
    div().child(item.name.clone())
}))

// Good
.children(items.iter().map(|item| {
    div().child(&item.name)
}))

// 2. Use conditional rendering wisely
// Bad: Creates element even when hidden
.child(
    div()
        .when(!visible, |this| this.hidden())
        .child(expensive_component())
)

// Good: Skips creation when not needed
.when(visible, |this| {
    this.child(expensive_component())
})

// 3. Cache computed values
struct MyView {
    items: Vec<Item>,
    filtered_items: Vec<Item>, // Cached
}
```

### 10. Common Patterns in CloudBridge

#### Dashboard Cards
```rust
fn render_card(&self, title: &str, value: &str, cx: &ViewContext<Self>) -> impl IntoElement {
    div()
        .w_full()
        .p_4()
        .rounded_lg()
        .border_1()
        .border_color(cx.theme().border)
        .bg(cx.theme().card_background)
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(title)
        )
        .child(
            div()
                .text_2xl()
                .font_weight(FontWeight::BOLD)
                .child(value)
        )
}
```

#### Loading States
```rust
fn render_loading(&self) -> impl IntoElement {
    div()
        .w_full()
        .h_full()
        .flex()
        .items_center()
        .justify_center()
        .child("Loading...")
}
```

#### Empty States
```rust
fn render_empty(&self, message: &str) -> impl IntoElement {
    div()
        .w_full()
        .h_full()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap_2()
        .child(
            div()
                .text_lg()
                .text_color(cx.theme().muted_foreground)
                .child(message)
        )
}
```

## Anti-Patterns to Avoid

1. **Don't use `unwrap()` in UI code** - Always handle errors gracefully
2. **Don't hold locks across await points** - Use channels instead
3. **Don't clone entire structs unnecessarily** - Use `Arc` or references
4. **Don't create deeply nested div hierarchies** - Extract helper methods
5. **Don't forget to call `cx.notify()`** - State changes won't render without it
6. **Don't hardcode pixel values** - Use theme spacing when possible
7. **Don't forget `.detach()` on spawned tasks** - Memory leaks otherwise

## Testing GPUI Components

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn test_component_creation(cx: &mut TestAppContext) {
        let view = cx.new_view(|cx| MyView::new(cx));
        // Test assertions
    }
}
```

## Resources

- GPUI Documentation: https://gpui.rs/
- GPUI Component Library: https://longbridge.github.io/gpui-component/
- CloudBridge Examples: See `src/ui/` directory
