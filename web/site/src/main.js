// Boots the WebAssembly build of CloudBridge.
//
// There is no framework and no bundler here: `wasm-bindgen --target web` emits
// an ES module, so the browser loads it directly. The only JavaScript the
// application needs is this file, and the only job it has is the platform
// input element below.

/**
 * `gpui_web` reads keyboard and IME input through a 1x1 transparent `<input>`
 * it appends to the body, and focuses that element when the window opens and
 * again on every pointerdown.
 *
 * On a touch-only device that raises the on-screen keyboard over the canvas,
 * which is unusable: typing into a canvas through an off-screen input is not
 * a thing anyone does on a phone. Such devices get the element marked
 * read-only with `inputmode="none"`, which iOS leaves the keyboard closed for
 * while keeping the element focusable, so the application still tracks window
 * activation and key events. Devices with a real pointer are left alone, so
 * desktop keyboards and IME composition behave exactly as before.
 */
const touchOnly =
  window.matchMedia?.('(hover: none) and (pointer: coarse)').matches ?? false;

function keepKeyboardClosed(node) {
  if (node.tagName !== 'INPUT') return;
  if (!touchOnly) return;

  node.readOnly = true;
  node.setAttribute('inputmode', 'none');

  if (document.activeElement === node) {
    node.blur();
  }
}

// Watched from before the module boots, so the element is handled as soon as
// gpui appends it rather than after the keyboard has had a chance to appear.
function watchPlatformInput() {
  document.querySelectorAll('body > input').forEach(keepKeyboardClosed);
  new MutationObserver((records) => {
    for (const record of records) {
      record.addedNodes.forEach(keepKeyboardClosed);
    }
  }).observe(document.body, { childList: true });
}

async function init() {
  const loading = document.getElementById('loading');
  watchPlatformInput();

  try {
    const wasm = await import('./wasm/cloudbridge.js');
    await wasm.default();
    // The demo seeds its own data and opens its own window; there is nothing
    // to pass in.
    await wasm.run();
    loading?.remove();
  } catch (error) {
    console.error('Failed to start CloudBridge:', error);
    if (loading) {
      loading.innerHTML = `
        <div class="error">
          <h2>CloudBridge could not start</h2>
          <p>${error?.message || error}</p>
          <p>See the browser console for the full error.</p>
        </div>`;
    }
  }
}

init();
