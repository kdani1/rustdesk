// PHASE 2 boundary: screen capture and synthetic input.
//
// This module is intentionally empty of behavior. It marks the seam where the
// next phase plugs in, so the phase-1 surface (list_peers / ls / upload /
// download / run / port_forward) stays self-contained and the phase-2 work has
// a clear home that does not disturb it.
//
// TODO(phase 2): screenshot
//   * Tool: `screenshot(peer_id, display)` -> PNG (returned as an MCP image
//     content block, base64).
//   * Open a DEFAULT_CONN session (video), wait for set_peer_info / first frame.
//   * Request a still via Session::take_screenshot(display, sid) (sends a
//     ScreenshotRequest); the reply arrives in
//     InvokeUiSession::handle_screenshot_resp(sid, msg) -- currently a no-op in
//     handler.rs. Route the image bytes into shared state keyed by `sid` and
//     wait for them there, mirroring the terminal/dir waiters.
//
// TODO(phase 2): mouse + keyboard input
//   * Tools: `mouse(peer_id, x, y, button, action)`, `type(peer_id, text)`,
//     `key(peer_id, key)`.
//   * Use the Session inherent methods already present:
//       - Session::send_mouse(mask, x, y, alt, ctrl, shift, command, ...)
//       - Session::input_string(&str)
//       - Session::input_key(name, down, press, alt, ctrl, shift, command)
//   * Mouse encoding (crate::common::input):
//       MOUSE_TYPE_MOVE = 0, MOUSE_TYPE_DOWN = 1, MOUSE_TYPE_UP = 2,
//       MOUSE_TYPE_WHEEL = 3;
//       MOUSE_BUTTON_LEFT = 1, MOUSE_BUTTON_RIGHT = 2, MOUSE_BUTTON_WHEEL = 4;
//       mask = (button << 3) | type.
//   * Key names map through crate::client KEY_MAP (VK_* entries).
//
// Keep phase-2 session management on plain std threads, exactly like phase 1.
