import 'dart:async';
import 'dart:convert';

import 'package:flutter/gestures.dart';
import 'package:flutter/widgets.dart';
import 'package:get/get.dart';

import '../common.dart';
import 'input_model.dart';
import 'platform_model.dart';

/// Meta Quest controller and pointer support for the mobile remote page.
///
/// The Android side (QuestInput.kt) forwards controller buttons and sticks as
/// "quest_input" method calls while a remote session is open on a Quest. They
/// are mapped here onto the existing mouse and key senders of [InputModel]:
///
///   right stick ........ move the remote cursor (frame-timed, ~60 Hz)
///   left stick ......... scroll wheel (vertical, or horizontal when pushed sideways)
///   triggers (L2/R2) ... left mouse button (held while held: drag works)
///   grips (L1/R1) ...... right mouse button
///   A .................. left mouse button      B ... right mouse button
///   X .................. Enter                  Y ... Esc
///   right stick press .. middle mouse button
///   left stick press ... toggle the input debug readout
///   D-pad / hat ........ arrow keys, D-pad centre = Enter
///
/// In "Quest pointer mode" (on by default on a Quest) the laser pointer and
/// direct touch are absolute: hover moves the remote cursor, press/release is a
/// left button press/release at that point, and moving while pressed drags.
/// See InputModel._questPointerEvent.

/// Verbose "[quest] ..." logging to the device log. Off by default (the events
/// are high-rate); flip to true and rebuild to debug. The on-screen debug
/// readout is separate and stays controllable at runtime (left-stick / menu).
const bool kQuestDebug = false;

class QuestInputModel {
  static const kOptionPointerMode = 'quest-pointer-mode';
  static const kOptionDebug = 'quest-input-debug';
  static const kOptionCursorOverlay = 'quest-cursor-overlay';
  static const kOptionAutoKeyboard = 'quest-auto-keyboard';

  // Android KeyEvent key codes.
  static const _kDpadUp = 19;
  static const _kDpadDown = 20;
  static const _kDpadLeft = 21;
  static const _kDpadRight = 22;
  static const _kDpadCenter = 23;
  static const _kButtonA = 96;
  static const _kButtonB = 97;
  static const _kButtonX = 99;
  static const _kButtonY = 100;
  static const _kButtonL1 = 102;
  static const _kButtonR1 = 103;
  static const _kButtonL2 = 104;
  static const _kButtonR2 = 105;
  static const _kButtonThumbL = 106;
  static const _kButtonThumbR = 107;

  /// Cursor speed at full stick deflection, in logical screen pixels per second.
  static const double kCursorSpeed = 900;

  /// Wheel ticks per second at full stick deflection.
  static const double kScrollTicksPerSec = 14;
  static const double _kStickDeadzone = 0.12;

  final InputModel input;
  QuestInputModel(this.input);

  final isQuest = false.obs;
  final pointerMode = true.obs;

  /// On-screen debug readout (the green status/event HUD). Off by default (it
  /// distracts in normal use); on only when the user enabled it, toggleable at
  /// runtime (left-stick click / Quest menu). Separate from the compile-time
  /// [kQuestDebug] logcat flag.
  final debug = false.obs;

  /// Draw the local software cursor in the remote view. Off by default (the
  /// Quest's own laser pointer is enough); toggleable from the Quest menu.
  final cursorOverlay = false.obs;

  /// Auto-open the soft keyboard when the remote cursor becomes a text caret
  /// (see CursorModel.remoteCursorIsText). On by default; toggleable.
  final autoKeyboard = true.obs;

  /// Called after a Quest pointer left click/drag is released (auto-keyboard hook), with the
  /// release position in local logical px.
  void Function(Offset localPos)? onLeftClick;
  /// Hover while no button is held (auto-keyboard disarm when the pointer leaves the click spot).
  void Function(Offset localPos)? onHover;
  final debugLines = <String>[].obs;
  final debugStatus = ''.obs;

  bool _attached = false;
  bool get attached => _attached;
  bool get pointerModeActive => isQuest.value && pointerMode.value;

  /// Sources that currently hold each mouse button; the button is down while the set is non-empty,
  /// so a laser click and a trigger press that arrive together send one press.
  final Map<MouseButtons, Set<String>> _held = {};

  double _lx = 0, _ly = 0, _rx = 0, _ry = 0;
  int _hatX = 0, _hatY = 0;
  bool _lt = false, _rt = false;
  Timer? _timer;
  int _lastTickUs = 0;
  double _scrollAccum = 0;
  int _lastStatusUs = 0;
  int _lastPointerLogUs = 0;

  Future<void> attach() async {
    if (!isAndroid) return;
    _attached = true;
    pointerMode.value =
        bind.mainGetLocalOption(key: kOptionPointerMode) != 'N';
    debug.value = bind.mainGetLocalOption(key: kOptionDebug) == 'Y';
    cursorOverlay.value =
        bind.mainGetLocalOption(key: kOptionCursorOverlay) == 'Y';
    autoKeyboard.value =
        bind.mainGetLocalOption(key: kOptionAutoKeyboard) != 'N';
    String model = '';
    try {
      final info = await platformFFI.invokeMethod('quest_get_info');
      if (info is Map) {
        isQuest.value = info['isQuest'] == true;
        model = '${info['manufacturer']} ${info['model']}';
      }
    } catch (e) {
      debugPrint('quest_get_info failed: $e');
    }
    if (!_attached || !isQuest.value) return;
    await _pushConfig();
    log('Quest input on ($model). Pointer mode ${pointerMode.value ? 'ON' : 'OFF'}. '
        'Left stick click hides this.');
  }

  Future<void> detach() async {
    if (!_attached) return;
    _attached = false;
    _stopTimer();
    _lx = _ly = _rx = _ry = 0;
    _setHat(0, 0);
    _lt = _rt = false;
    _releaseAll();
    if (isQuest.value) {
      try {
        await platformFFI
            .invokeMethod('quest_input_config', {'active': false, 'debug': false});
      } catch (_) {}
    }
  }

  Future<void> _pushConfig() async {
    try {
      await platformFFI.invokeMethod('quest_input_config',
          {'active': _attached && isQuest.value, 'debug': debug.value});
    } catch (e) {
      debugPrint('quest_input_config failed: $e');
    }
  }

  void setPointerMode(bool v) {
    pointerMode.value = v;
    bind.mainSetLocalOption(key: kOptionPointerMode, value: v ? 'Y' : 'N');
    // Leaving pointer mode with the laser held must not leave the button stuck.
    _releaseSource('pointer');
    log('Pointer mode ${v ? 'ON' : 'OFF'}');
  }

  void setCursorOverlay(bool v) {
    cursorOverlay.value = v;
    bind.mainSetLocalOption(key: kOptionCursorOverlay, value: v ? 'Y' : 'N');
  }

  void setAutoKeyboard(bool v) {
    autoKeyboard.value = v;
    bind.mainSetLocalOption(key: kOptionAutoKeyboard, value: v ? 'Y' : 'N');
  }

  void setDebug(bool v) {
    debug.value = v;
    bind.mainSetLocalOption(key: kOptionDebug, value: v ? 'Y' : 'N');
    if (!v) {
      debugLines.clear();
      debugStatus.value = '';
    }
    unawaited(_pushConfig());
  }

  void log(String line) {
    if (!debug.value) return;
    if (kQuestDebug) debugPrint('[quest] $line');
    debugLines.add(line);
    if (debugLines.length > 7) debugLines.removeRange(0, debugLines.length - 7);
  }

  /// Flutter-side view of a pointer event, for the readout (what Flutter made of the event).
  void logPointer(PointerEvent e, String phase) {
    if (!debug.value) return;
    final nowUs = DateTime.now().microsecondsSinceEpoch;
    if ((phase == 'hover' || phase == 'move') && nowUs - _lastPointerLogUs < 150000) {
      return;
    }
    _lastPointerLogUs = nowUs;
    log('flutter $phase kind=${e.kind.name} btn=${e.buttons} '
        '(${e.localPosition.dx.toStringAsFixed(0)},${e.localPosition.dy.toStringAsFixed(0)})');
  }

  /// Entry point for the "quest_input" method call from Android.
  void onPlatformEvent(dynamic arguments) {
    if (arguments is! Map) return;
    final kind = arguments['k'];
    if (kind == 'dbg') {
      log('${arguments['t']}');
      return;
    }
    if (!_attached || !isQuest.value) return;
    if (kind == 'key') {
      _onKey((arguments['code'] as num).toInt(), arguments['down'] == true,
          (arguments['repeat'] as num?)?.toInt() ?? 0);
    } else if (kind == 'axes') {
      _onAxes(arguments);
    }
  }

  // ---- buttons ----

  int get heldButtonsMask {
    int m = 0;
    if (_held[MouseButtons.left]?.isNotEmpty ?? false) m |= kPrimaryMouseButton;
    if (_held[MouseButtons.right]?.isNotEmpty ?? false) m |= kSecondaryMouseButton;
    if (_held[MouseButtons.wheel]?.isNotEmpty ?? false) m |= kMiddleMouseButton;
    return m;
  }

  void button(MouseButtons b, String source, bool down) {
    final s = _held.putIfAbsent(b, () => <String>{});
    if (down) {
      if (s.add(source) && s.length == 1) {
        unawaited(input.sendMouse('down', b));
      }
    } else {
      if (s.remove(source) && s.isEmpty) {
        unawaited(input.sendMouse('up', b));
      }
    }
  }

  void _releaseSource(String source) {
    for (final b in _held.keys.toList()) {
      if (_held[b]!.contains(source)) button(b, source, false);
    }
  }

  void _releaseAll() {
    for (final e in _held.entries) {
      if (e.value.isNotEmpty) {
        e.value.clear();
        unawaited(input.sendMouse('up', e.key));
      }
    }
  }

  void _key(String name, bool down) {
    input.inputKey(name, down: down, press: false);
  }

  void _onKey(int code, bool down, int repeat) {
    if (repeat > 0) {
      // Auto-repeat: only the arrows repeat on the remote side; buttons stay held.
      final arrow = _arrowName(code);
      if (arrow != null && down) _key(arrow, true);
      return;
    }
    switch (code) {
      case _kButtonR2:
        button(MouseButtons.left, 'R2', down);
        break;
      case _kButtonL2:
        button(MouseButtons.left, 'L2', down);
        break;
      case _kButtonR1:
        button(MouseButtons.right, 'R1', down);
        break;
      case _kButtonL1:
        button(MouseButtons.right, 'L1', down);
        break;
      case _kButtonA:
        button(MouseButtons.left, 'A', down);
        break;
      case _kButtonB:
        button(MouseButtons.right, 'B', down);
        break;
      case _kButtonThumbR:
        button(MouseButtons.wheel, 'R3', down);
        break;
      case _kButtonX:
        _key('VK_ENTER', down);
        break;
      case _kButtonY:
        _key('VK_ESCAPE', down);
        break;
      case _kDpadCenter:
        _key('VK_ENTER', down);
        break;
      case _kButtonThumbL:
        if (down) {
          final v = !debug.value;
          if (v) {
            setDebug(true);
            log('Debug readout ON (left stick click to hide)');
          } else {
            setDebug(false);
          }
        }
        break;
      default:
        final arrow = _arrowName(code);
        if (arrow != null) _key(arrow, down);
    }
  }

  static String? _arrowName(int code) {
    switch (code) {
      case _kDpadUp:
        return 'VK_UP';
      case _kDpadDown:
        return 'VK_DOWN';
      case _kDpadLeft:
        return 'VK_LEFT';
      case _kDpadRight:
        return 'VK_RIGHT';
    }
    return null;
  }

  // ---- axes ----

  void _onAxes(Map a) {
    double d(String k) => (a[k] as num?)?.toDouble() ?? 0.0;
    _lx = d('lx');
    _ly = d('ly');
    _rx = d('rx');
    _ry = d('ry');
    _setHat(d('hx').round(), d('hy').round());
    // Analog triggers, with hysteresis so a half-pressed trigger does not chatter.
    final lt = d('lt'), rt = d('rt');
    if (!_lt && lt > 0.6) {
      _lt = true;
      button(MouseButtons.left, 'LT', true);
    } else if (_lt && lt < 0.35) {
      _lt = false;
      button(MouseButtons.left, 'LT', false);
    }
    if (!_rt && rt > 0.6) {
      _rt = true;
      button(MouseButtons.left, 'RT', true);
    } else if (_rt && rt < 0.35) {
      _rt = false;
      button(MouseButtons.left, 'RT', false);
    }
    if (debug.value) {
      final nowUs = DateTime.now().microsecondsSinceEpoch;
      if (nowUs - _lastStatusUs > 100000) {
        _lastStatusUs = nowUs;
        String f(double v) => v.toStringAsFixed(2);
        debugStatus.value =
            'L(${f(_lx)},${f(_ly)}) R(${f(_rx)},${f(_ry)}) hat($_hatX,$_hatY) '
            'trig(${f(lt)},${f(rt)}) buttons=$heldButtonsMask';
      }
    }
    if (_lx != 0 || _ly != 0 || _rx != 0 || _ry != 0) {
      _ensureTimer();
    }
  }

  void _setHat(int x, int y) {
    if (x != _hatX) {
      if (_hatX < 0) _key('VK_LEFT', false);
      if (_hatX > 0) _key('VK_RIGHT', false);
      if (x < 0) _key('VK_LEFT', true);
      if (x > 0) _key('VK_RIGHT', true);
      _hatX = x;
    }
    if (y != _hatY) {
      if (_hatY < 0) _key('VK_UP', false);
      if (_hatY > 0) _key('VK_DOWN', false);
      if (y < 0) _key('VK_UP', true);
      if (y > 0) _key('VK_DOWN', true);
      _hatY = y;
    }
  }

  static double _curve(double v) {
    final mag = ((v.abs() - _kStickDeadzone) / (1 - _kStickDeadzone)).clamp(0.0, 1.0);
    return (v < 0 ? -1 : 1) * mag * mag;
  }

  void _ensureTimer() {
    if (_timer != null) return;
    _lastTickUs = DateTime.now().microsecondsSinceEpoch;
    _timer = Timer.periodic(const Duration(milliseconds: 16), (_) => _tick());
  }

  void _stopTimer() {
    _timer?.cancel();
    _timer = null;
    _scrollAccum = 0;
  }

  void _tick() {
    if (!_attached || (_lx == 0 && _ly == 0 && _rx == 0 && _ry == 0)) {
      _stopTimer();
      return;
    }
    final nowUs = DateTime.now().microsecondsSinceEpoch;
    final dt = ((nowUs - _lastTickUs) / 1e6).clamp(0.0, 0.05);
    _lastTickUs = nowUs;
    final ffi = input.parent.target;
    if (ffi == null || !input.keyboardPerm || input.isViewCamera) return;

    // Right stick: cursor. updatePan moves the local cursor, pans a zoomed canvas, sends the move.
    if (_rx != 0 || _ry != 0) {
      final dx = _curve(_rx) * kCursorSpeed * dt;
      final dy = _curve(_ry) * kCursorSpeed * dt;
      if (dx != 0 || dy != 0) {
        unawaited(ffi.cursorModel.updatePan(Offset(dx, dy), Offset.zero, false));
      }
    }

    // Left stick: wheel. Dominant axis only, so a slightly diagonal push does not scroll sideways.
    final vertical = _ly.abs() >= _lx.abs();
    final v = vertical ? _ly : _lx;
    if (v != 0) {
      _scrollAccum += _curve(v).abs() * kScrollTicksPerSec * dt;
      if (_scrollAccum >= 1) {
        _scrollAccum -= 1;
        // Same sign convention as InputModel.onPointerSignalImage: stick down/right = scroll
        // down/right = negative wheel value.
        final tick = v > 0 ? -1 : 1;
        _wheel(vertical ? 0 : tick, vertical ? tick : 0);
      }
    } else {
      _scrollAccum = 0;
    }
  }

  void _wheel(int x, int y) {
    unawaited(bind.sessionSendMouse(
        sessionId: input.sessionId,
        msg: json.encode(input.modify({'type': 'wheel', 'x': '$x', 'y': '$y'}))));
  }
}
