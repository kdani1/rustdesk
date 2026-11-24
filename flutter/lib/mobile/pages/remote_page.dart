import 'dart:async';

import 'package:flutter/gestures.dart' show PointerDeviceKind;
import 'package:flutter/material.dart';
import 'package:flutter/rendering.dart' show RenderProxyBox;
import 'package:flutter/services.dart';
import 'package:flutter_hbb/common/shared_state.dart';
import 'package:flutter_hbb/common/widgets/toolbar.dart';
import 'package:flutter_hbb/consts.dart';
import 'package:flutter_hbb/mobile/widgets/floating_mouse.dart';
import 'package:flutter_hbb/mobile/widgets/floating_mouse_widgets.dart';
import 'package:flutter_hbb/mobile/widgets/gesture_help.dart';
import 'package:flutter_hbb/models/chat_model.dart';
import 'package:flutter_keyboard_visibility/flutter_keyboard_visibility.dart';
import 'package:flutter_svg/svg.dart';
import 'package:get/get.dart';
import 'package:provider/provider.dart';

import '../../common.dart';
import '../../common/widgets/overlay.dart';
import '../../common/widgets/dialog.dart';
import '../../common/widgets/remote_input.dart';
import '../../models/input_model.dart';
import '../../models/model.dart';
import '../../models/platform_model.dart';
import '../../utils/image.dart';
import '../widgets/dialog.dart';
import '../widgets/custom_scale_widget.dart';

final initText = '1' * 1024;

// Workaround for Android (default input method, Microsoft SwiftKey keyboard) when using physical keyboard.
// When connecting a physical keyboard, `KeyEvent.physicalKey.usbHidUsage` are wrong is using Microsoft SwiftKey keyboard.
// https://github.com/flutter/flutter/issues/159384
// https://github.com/flutter/flutter/issues/159383
void _disableAndroidSoftKeyboard({bool? isKeyboardVisible}) {
  if (isAndroid && !gFFI.inputModel.quest.isQuest.value) {
    if (isKeyboardVisible != true) {
      // `enable_soft_keyboard` will be set to `true` when clicking the keyboard icon, in `openKeyboard()`.
      gFFI.invokeMethod("enable_soft_keyboard", false);
    }
  }
}

class RemotePage extends StatefulWidget {
  RemotePage(
      {Key? key,
      required this.id,
      this.password,
      this.isSharedPassword,
      this.forceRelay,
      this.panelDisplay,
      this.panelCachedPeerData})
      : super(key: key);

  /// Android display panel (DisplayActivity): this page shows only this display of a
  /// connection the main panel owns, attached with the main engine's cached peer data.
  final int? panelDisplay;
  final String? panelCachedPeerData;

  final String id;
  final String? password;
  final bool? isSharedPassword;
  final bool? forceRelay;

  @override
  State<RemotePage> createState() => _RemotePageState(id);
}

class _RemotePageState extends State<RemotePage> with WidgetsBindingObserver {
  Timer? _timer;
  bool _showBar = !isWebDesktop;
  bool _showGestureHelp = false;
  String _value = '';
  Orientation? _currentOrientation;
  final _uniqueKey = UniqueKey();
  Timer? _iosKeyboardWorkaroundTimer;

  final _blockableOverlayState = BlockableOverlayState();

  final keyboardVisibilityController = KeyboardVisibilityController();
  late final StreamSubscription<bool> keyboardSubscription;
  final FocusNode _mobileFocusNode = FocusNode();
  final FocusNode _physicalFocusNode = FocusNode();
  var _showEdit = false; // use soft keyboard

  Worker? _waylandKeyboardGateWorker;
  // Quest auto-keyboard: a left click arms ONE open (see _onQuestLeftClick).
  Worker? _questAutoKbdWorker;
  Timer? _questArmTimer;
  bool _questKbdArmed = false;
  // A newer Windows host answers every click with what got the keyboard focus (see
  // _onHostTextFocus). Known once its first message arrived; until then, and with older
  // hosts, the remote cursor shape decides as before.
  bool _hostAnswersFocus = false;
  bool _questAwaitHost = false; // armed, waiting for the host's answer to this click
  bool _questClickWasText = false; // the remote cursor was a caret when that click was made
  bool _questLeftSpot = false; // the pointer left the click spot while waiting for the host
  final Stopwatch _questArmClock = Stopwatch(); // since that click
  // The host cannot answer a click sooner than its first sample (90 ms after it got the
  // click): anything earlier is the late answer to an older click.
  static const int _kHostAnswerMinMs = 80;
  Offset _questArmPos = Offset.zero;
  static const _kQuestKbdArmWindow = Duration(milliseconds: 1200);
  static const double _kQuestKbdArmSlop = 24.0; // same as input_model long-press slop
  Timer? _kbdVisFocusTimer; // onSoftKeyboardChanged's own; must not cancel _timer
  bool _waylandKeyboardGateInitialized = false;

  InputModel get inputModel => gFFI.inputModel;
  SessionID get sessionId => gFFI.sessionId;
  bool get _isPanel => widget.panelDisplay != null;

  final TextEditingController _textController =
      TextEditingController(text: initText);

  _RemotePageState(String id) {
    initSharedStates(id);
    gFFI.chatModel.voiceCallStatus.value = VoiceCallStatus.notStarted;
    gFFI.dialogManager.loadMobileActionsOverlayVisible();
  }

  /// Clean-up of the peer option "virtual-display" (one entry per virtual display that is
  /// plugged in again at every connection). Older builds added an entry for every press of
  /// "+", also when the host refused it, so a peer can hold more entries than displays that
  /// ever existed, and all of them would be requested in a burst at the next connection. This
  /// build stores the count the host confirms, never more than 4 (see toolbar.dart and
  /// FfiModel.handlePlatformAdditions), so a longer list is such a leftover: cut it to two.
  void _trimStoredVirtualDisplays() {
    try {
      final stored = bind
          .mainGetPeerOptionSync(id: widget.id, key: 'virtual-display')
          .split(',')
          .where((e) => e.isNotEmpty)
          .toList();
      // Every entry is one more display on top of the host's own first one (a headless
      // host plugs that one in by itself), and the core asks for them at the first frame.
      // A session starts with the main display alone: none. The display windows of the last
      // session are restored on request (AndroidDisplayPanels).
      if (isAndroid && stored.isNotEmpty) {
        bind.mainSetPeerOptionSync(
            id: widget.id, key: 'virtual-display', value: '');
      } else if (stored.length > 4) {
        bind.mainSetPeerOptionSync(
            id: widget.id,
            key: 'virtual-display',
            value: stored.take(2).join(','));
      }
    } catch (e) {
      debugPrint('virtual-display clean-up failed: $e');
    }
  }

  @override
  void initState() {
    super.initState();
    if (_isPanel) {
      _showBar = false; // only the small handle: the window is all picture
      _hostAnswersFocus = kDisplayPanelArgs?['hostAnswersFocus'] == true;
      gFFI.ffiModel.updateEventListener(sessionId, widget.id);
      gFFI.start(widget.id,
          tabWindowId: 0, // not null: attach to the existing connection
          display: widget.panelDisplay,
          displays: [widget.panelDisplay!],
          cachedPeerDataJson: widget.panelCachedPeerData);
    } else {
      gFFI.inputModel.quest.hostAnswersFocus = false; // the next host may be an older one
      androidDisplayPanels.newConnection();
      // While the display windows are being tuned: the viewer's own timing lines
      // ("LATENCY client ...", docs/LATENCY-METRICS.md) go to logcat.
      bind.mainSetLocalOption(key: 'enable-latency-log', value: 'Y');
      _trimStoredVirtualDisplays();
      gFFI.ffiModel.updateEventListener(sessionId, widget.id);
      gFFI.start(
        widget.id,
        password: widget.password,
        isSharedPassword: widget.isSharedPassword,
        forceRelay: widget.forceRelay,
      );
    }
    WidgetsBinding.instance.addPostFrameCallback((_) {
      SystemChrome.setEnabledSystemUIMode(SystemUiMode.manual, overlays: []);
      if (_isPanel) return; // already connected
      gFFI.dialogManager
          .showLoading(translate('Connecting...'), onCancel: closeConnection);
    });
    WakelockManager.enable(_uniqueKey);
    _physicalFocusNode.requestFocus();
    gFFI.inputModel.listenToMouse(true);
    gFFI.inputModel.loadDragScrollOptions();
    unawaited(gFFI.inputModel.quest.attach());
    // Quest auto-keyboard: a left click arms one open, fired when the remote cursor is (or,
    // while the pointer stays put, becomes) a text caret. Never auto-closed.
    _questAutoKbdWorker =
        ever<bool>(gFFI.cursorModel.remoteCursorIsText, (isText) {
      if (isText && _questKbdArmed && !_questAwaitHost) _questKbdFire('shape');
    });
    gFFI.inputModel.quest.onLeftClick = _onQuestLeftClick;
    gFFI.inputModel.quest.onHover = _onQuestHover;
    gFFI.inputModel.quest.onHostTextFocus = _onHostTextFocus;
    gFFI.qualityMonitorModel.checkShowQualityMonitor(sessionId);
    keyboardSubscription =
        keyboardVisibilityController.onChange.listen(onSoftKeyboardChanged);
    gFFI.chatModel
        .changeCurrentKey(MessageKey(widget.id, ChatModel.clientModeID));
    _blockableOverlayState.applyFfi(gFFI);
    gFFI.imageModel.addCallbackOnFirstImage((String peerId) {
      gFFI.recordingModel
          .updateStatus(bind.sessionGetIsRecording(sessionId: gFFI.sessionId));
      if (gFFI.recordingModel.start) {
        showToast(translate('Automatically record outgoing sessions'));
      }
      _disableAndroidSoftKeyboard(
          isKeyboardVisible: keyboardVisibilityController.isVisible);
    });
    WidgetsBinding.instance.addObserver(this);
    WidgetsBinding.instance.addPostFrameCallback((_) {
      if (mounted) _windowSize = MediaQueryData.fromView(View.of(context)).size;
    });

    inputModel.keyboardInputAllowed = true;

    // Wayland sessions may use clipboard-based text input on the controlled side.
    // Require explicit user confirmation before allowing soft-keyboard and
    // clipboard-assisted text input. Physical keyboard events are not gated here.
    _waylandKeyboardGateWorker = ever(gFFI.ffiModel.pi.isSet, (bool isSet) {
      if (isSet) {
        _initWaylandKeyboardGateIfNeeded();
      }
    });
    if (gFFI.ffiModel.pi.isSet.value) {
      _initWaylandKeyboardGateIfNeeded();
    }
  }

  @override
  Future<void> dispose() async {
    WidgetsBinding.instance.removeObserver(this);
    _windowResizeTimer?.cancel();
    // The extra display windows belong to this connection.
    if (!_isPanel) unawaited(androidDisplayPanels.closeAll());
    // Gamepad mode: let go of the controllers. The off = true this sends usually arrives after
    // `sessionClose` below has removed the session (the platform call runs on Kotlin's main
    // looper later), so it is dropped; the host unplugs the pad anyway when the connection
    // closes (gamepad::release). Not awaited, see the comment below.
    if (!_isPanel) gFFI.inputModel.quest.stopGamepadMode();
    // Close the session up-front. `gFFI.close()` below only calls `sessionClose`
    // after several awaits (canvas save, image update, the `enable_soft_keyboard`
    // platform call), so if the app is backgrounded while this page is disposing,
    // dispose can be suspended before reaching it and the connection is never torn
    // down. The reconnect then re-attaches to the leaked session and is stuck on
    // "Connecting...". Dispatching it here makes teardown happen synchronously on
    // pop; the `sessionClose` in `gFFI.close()` becomes a no-op once removed.
    unawaited(bind.sessionClose(sessionId: sessionId));
    // https://github.com/flutter/flutter/issues/64935
    super.dispose();
    gFFI.dialogManager.hideMobileActionsOverlay(store: false);
    gFFI.canvasModel.mobileBottomBarHeight = 0;
    gFFI.inputModel.listenToMouse(false);
    unawaited(gFFI.inputModel.quest.detach());
    gFFI.imageModel.disposeImage();
    gFFI.cursorModel.disposeImages();
    await gFFI.invokeMethod("enable_soft_keyboard", true);
    _mobileFocusNode.dispose();
    _physicalFocusNode.dispose();
    clearWaylandKeyboardPromptSuppressedForConnection(sessionId.toString());
    _waylandKeyboardGateWorker?.dispose();
    _questAutoKbdWorker?.dispose();
    gFFI.inputModel.quest.onLeftClick = null;
    gFFI.inputModel.quest.onHover = null;
    gFFI.inputModel.quest.onHostTextFocus = null;
    _questArmTimer?.cancel();
    _kbdVisFocusTimer?.cancel();
    inputModel.keyboardInputAllowed = true;
    await gFFI.close();
    _timer?.cancel();
    _iosKeyboardWorkaroundTimer?.cancel();
    gFFI.dialogManager.dismissAll();
    await SystemChrome.setEnabledSystemUIMode(SystemUiMode.manual,
        overlays: SystemUiOverlay.values);
    WakelockManager.disable(_uniqueKey);
    await keyboardSubscription.cancel();
    removeSharedStates(widget.id);
    // `on_voice_call_closed` should be called when the connection is ended.
    // The inner logic of `on_voice_call_closed` will check if the voice call is active.
    // Only one client is considered here for now.
    gFFI.chatModel.onVoiceCallClosed("End connection");
  }

  // The window size the image was last fitted for (see didChangeMetrics).
  Size? _windowSize;
  Timer? _windowResizeTimer;

  /// The window changed size without turning (a Quest panel was resized, e.g. made wider
  /// for several displays side by side): fit the image again. Only the window's own size
  /// counts, not the keyboard inset, which also arrives here. A turn of the device is
  /// handled by the OrientationBuilder in build().
  @override
  void didChangeMetrics() {
    _windowResizeTimer?.cancel();
    _windowResizeTimer = Timer(const Duration(milliseconds: 150), () {
      if (!mounted) return;
      final size = MediaQueryData.fromView(View.of(context)).size;
      final last = _windowSize;
      _windowSize = size;
      if (last == null || last == size) return;
      final canvas = gFFI.canvasModel;
      canvas.setMobileBottomBarHeight(canvas.mobileBottomBarHeight);
    });
  }

  @override
  void didChangeAppLifecycleState(AppLifecycleState state) {
    if (state != AppLifecycleState.resumed) inputModel.quest.releaseSticks();
    if (state == AppLifecycleState.resumed) {
      trySyncClipboard();
    } else if (inputModel.quest.isQuest.value) {
      // Android restarts input on resume; the Quest IME may re-commit the field text.
      _value = '';
      _textController.clear();
    }
  }

  // For client side
  // When swithing from other app to this app, try to sync clipboard.
  void trySyncClipboard() {
    gFFI.invokeMethod("try_sync_clipboard");
  }

  bool _shouldGateKeyboardForWayland() {
    if (!(isAndroid || isIOS)) return false;
    final pi = gFFI.ffiModel.pi;
    return pi.platform == kPeerPlatformLinux && pi.isWayland;
  }

  void _initWaylandKeyboardGateIfNeeded() {
    if (!mounted) return;
    if (_waylandKeyboardGateInitialized) return;
    if (!_shouldGateKeyboardForWayland()) return;

    _waylandKeyboardGateInitialized = true;

    final allowWaylandKeyboard =
        mainGetPeerBoolOptionSync(widget.id, kPeerOptionAllowWaylandKeyboard);
    if (!shouldShowWaylandKeyboardPrompt(
      connectionId: sessionId.toString(),
      isWaylandPeer: _shouldGateKeyboardForWayland(),
      allowWaylandKeyboardRemembered: allowWaylandKeyboard,
    )) {
      inputModel.keyboardInputAllowed = true;
      return;
    }

    inputModel.keyboardInputAllowed = false;

    // Ensure soft keyboard is not active before user confirms.
    _showEdit = false;
    gFFI.invokeMethod("enable_soft_keyboard", false);
    _mobileFocusNode.unfocus();
    _physicalFocusNode.requestFocus();
    setState(() {});
  }

  // to-do: It should be better to use transparent color instead of the bgColor.
  // But for now, the transparent color will cause the canvas to be white.
  // I'm sure that the white color is caused by the Overlay widget in BlockableOverlay.
  // But I don't know why and how to fix it.
  Widget emptyOverlay(Color bgColor) => BlockableOverlay(
        /// the Overlay key will be set with _blockableOverlayState in BlockableOverlay
        /// see override build() in [BlockableOverlay]
        state: _blockableOverlayState,
        underlying: Container(
          color: bgColor,
        ),
      );

  void onSoftKeyboardChanged(bool visible) {
    inputModel.quest.log('kbdVis=$visible');
    if (!visible) {
      SystemChrome.setEnabledSystemUIMode(SystemUiMode.manual, overlays: []);
      // [pi.version.isNotEmpty] -> check ready or not, avoid login without soft-keyboard
      if (gFFI.chatModel.chatWindowOverlayEntry == null &&
          gFFI.ffiModel.pi.version.isNotEmpty &&
          !inputModel.quest.isQuest.value) {
        gFFI.invokeMethod("enable_soft_keyboard", false);
      }

      // Workaround for iOS: physical keyboard input fails after virtual keyboard is hidden
      // https://github.com/flutter/flutter/issues/39900
      // https://github.com/rustdesk/rustdesk/discussions/11843#discussioncomment-13499698 - Virtual keyboard issue
      if (isIOS) {
        _iosKeyboardWorkaroundTimer?.cancel();
        _iosKeyboardWorkaroundTimer = Timer(Duration(milliseconds: 100), () {
          if (!mounted) return;
          _physicalFocusNode.unfocus();
          _iosKeyboardWorkaroundTimer = Timer(Duration(milliseconds: 50), () {
            if (!mounted) return;
            _physicalFocusNode.requestFocus();
          });
        });
      }
    } else {
      _iosKeyboardWorkaroundTimer?.cancel();
      _iosKeyboardWorkaroundTimer = null;
      _kbdVisFocusTimer?.cancel();
      _kbdVisFocusTimer = Timer(kMobileDelaySoftKeyboardFocus, () {
        if (!mounted) return;
        SystemChrome.setEnabledSystemUIMode(SystemUiMode.manual,
            overlays: SystemUiOverlay.values);
        _mobileFocusNode.requestFocus();
      });
    }
    // update for Scaffold
    setState(() {});
  }

  void _handleIOSSoftKeyboardInput(String newValue) {
    var oldValue = _value;
    _value = newValue;
    var i = newValue.length - 1;
    for (; i >= 0 && newValue[i] != '1'; --i) {}
    var j = oldValue.length - 1;
    for (; j >= 0 && oldValue[j] != '1'; --j) {}
    if (i < j) j = i;
    var subNewValue = newValue.substring(j + 1);
    var subOldValue = oldValue.substring(j + 1);

    // get common prefix of subNewValue and subOldValue
    var common = 0;
    for (;
        common < subOldValue.length &&
            common < subNewValue.length &&
            subNewValue[common] == subOldValue[common];
        ++common) {}

    // get newStr from subNewValue
    var newStr = "";
    if (subNewValue.length > common) {
      newStr = subNewValue.substring(common);
    }

    // Set the value to the old value and early return if is still composing. (1 && 2)
    // 1. The composing range is valid
    // 2. The new string is shorter than the composing range.
    if (_textController.value.isComposingRangeValid) {
      final composingLength = _textController.value.composing.end -
          _textController.value.composing.start;
      if (composingLength > newStr.length) {
        _value = oldValue;
        return;
      }
    }

    // Delete the different part in the old value.
    for (i = 0; i < subOldValue.length - common; ++i) {
      inputModel.inputKey('VK_BACK');
    }

    // Input the new string.
    if (newStr.length > 1) {
      bind.sessionInputString(sessionId: sessionId, value: newStr);
    } else {
      inputChar(newStr);
    }
  }

  void _handleNonIOSSoftKeyboardInput(String newValue) {
    var oldValue = _value;
    _value = newValue;
    if (oldValue.isNotEmpty &&
        newValue.isNotEmpty &&
        oldValue[0] == '1' &&
        newValue[0] != '1') {
      // clipboard
      oldValue = '';
    }
    if (newValue.length == oldValue.length) {
      // ?
    } else if (newValue.length < oldValue.length) {
      final char = 'VK_BACK';
      inputModel.inputKey(char);
    } else {
      final content = newValue.substring(oldValue.length);
      if (content.length > 1) {
        if (oldValue != '' &&
            content.length == 2 &&
            (content == '""' ||
                content == '()' ||
                content == '[]' ||
                content == '<>' ||
                content == "{}" ||
                content == '”“' ||
                content == '《》' ||
                content == '（）' ||
                content == '【】')) {
          // can not only input content[0], because when input ], [ are also auo insert, which cause ] never be input
          bind.sessionInputString(sessionId: sessionId, value: content);
          inputModel.releaseOneShotModifiers();
          if (!inputModel.quest.isQuest.value) _openKeyboardUnlocked();
          return;
        }
        bind.sessionInputString(sessionId: sessionId, value: content);
        // Dictation, swipe or a suggestion: no modifiers go with a string, but it is still
        // the "next key press" for a one-shot modifier.
        inputModel.releaseOneShotModifiers();
      } else {
        inputChar(content);
      }
    }
  }

  // handle mobile virtual keyboard
  void handleSoftKeyboardInput(String newValue) {
    if (!inputModel.keyboardInputAllowed) {
      return;
    }
    if (isIOS) {
      _handleIOSSoftKeyboardInput(newValue);
    } else {
      _handleNonIOSSoftKeyboardInput(newValue);
    }
  }

  void inputChar(String char) {
    if (!inputModel.keyboardInputAllowed) {
      return;
    }
    if (char == '\n') {
      char = 'VK_RETURN';
    } else if (char == ' ') {
      char = 'VK_SPACE';
    }
    inputModel.inputKey(char);
  }

  /// A Quest left click arms the auto-keyboard for ONE open: it fires now if the remote cursor
  /// is already a caret (the laser hovered there first), or when it turns into one before the
  /// window ends while the pointer stays put (direct touch / slow link). Firing disarms, so one
  /// click opens at most once and I-beam flapping (pointer hidden while typing) never re-opens.
  void _onQuestLeftClick(Offset pos) {
    if (!mounted) return;
    final quest = inputModel.quest;
    final String? skip = !quest.isQuest.value
        ? 'not-quest'
        : !quest.autoKeyboard.value
            ? 'toggle-off'
            : !gFFI.ffiModel.keyboard
                ? 'no-kbd-perm'
                : null;
    if (skip != null) {
      quest.log('autoKbd[click] -> $skip');
      return;
    }
    if (_hostAnswersFocus) {
      // The text cursor shows over any selectable text; the host knows whether the click
      // really landed in a text field. Wait for its answer instead.
      _questKbdArmed = true;
      _questAwaitHost = true;
      _questArmPos = pos;
      _questClickWasText = gFFI.cursorModel.remoteCursorIsText.value;
      _questLeftSpot = false;
      _questArmClock
        ..reset()
        ..start();
      _questArmTimer?.cancel();
      _questArmTimer = Timer(_kQuestKbdArmWindow, () {
        // A host that stays silent must never be worse than one that cannot answer.
        if (_questAwaitHost) _questHostUnknown();
        _questKbdDisarm(_questLeftSpot ? 'moved' : 'expired');
      });
      quest.log('autoKbd[click] -> waiting for the host');
      return;
    }
    if (gFFI.cursorModel.remoteCursorIsText.value) {
      _questKbdFire('click'); // laser hovered the caret before clicking
      return;
    }
    // Direct touch / slow link: the caret shape may arrive after the click.
    _questKbdArmed = true;
    _questArmPos = pos;
    _questArmTimer?.cancel();
    _questArmTimer =
        Timer(_kQuestKbdArmWindow, () => _questKbdDisarm('expired'));
    quest.log('autoKbd[click] text=false -> armed');
  }

  void _onQuestHover(Offset pos) {
    if (!_questKbdArmed ||
        (pos - _questArmPos).distance <= _kQuestKbdArmSlop) {
      return;
    }
    if (_questAwaitHost) {
      // The host's answer is about the click, not about where the pointer is now: keep
      // waiting. Only the cursor-shape fallback must not use a later hover.
      _questLeftSpot = true;
      return;
    }
    _questKbdDisarm('moved'); // a caret under a later hover is not the clicked field
  }

  /// The host's answer to the last click (Misc.text_input_focus), or its first message that
  /// only announces that it answers at all (state 0, nothing armed yet).
  void _onHostTextFocus(int state) {
    if (!mounted) return;
    _hostAnswersFocus = true;
    inputModel.quest.hostAnswersFocus = true;
    debugPrint('[quest] host text focus: $state '
        '(armed=$_questKbdArmed waiting=$_questAwaitHost)');
    if (!_questKbdArmed || !_questAwaitHost) return;
    if (_questArmClock.elapsedMilliseconds < _kHostAnswerMinMs) return;
    if (state == 2) {
      _questKbdFire('host');
    } else if (state == 1) {
      _questKbdDisarm('host: not a text field');
    } else {
      _questHostUnknown();
    }
  }

  /// The host could not tell (no accessibility data), or never answered: the cursor shape
  /// decides, exactly as with a host that does not answer at all.
  void _questHostUnknown() {
    _questAwaitHost = false;
    if (_questClickWasText ||
        (!_questLeftSpot && gFFI.cursorModel.remoteCursorIsText.value)) {
      _questKbdFire('host unknown, shape');
    } else if (_questLeftSpot) {
      _questKbdDisarm('moved');
    }
    // else it stays armed: the cursor may still turn into a caret within the window.
  }

  void _questKbdDisarm(String why) {
    if (_questKbdArmed) inputModel.quest.log('autoKbd armed -> $why');
    _questKbdArmed = false;
    _questAwaitHost = false;
    _questArmTimer?.cancel();
  }

  void _questKbdFire(String from) {
    _questKbdArmed = false;
    _questAwaitHost = false;
    _questArmTimer?.cancel();
    if (!mounted) return;
    inputModel.quest.log('autoKbd[$from] -> OPEN');
    openKeyboard(); // keeps the Wayland gate
  }

  void openKeyboard() {
    final allowWaylandKeyboard =
        mainGetPeerBoolOptionSync(widget.id, kPeerOptionAllowWaylandKeyboard);
    if (shouldShowWaylandKeyboardPrompt(
      connectionId: sessionId.toString(),
      isWaylandPeer: _shouldGateKeyboardForWayland(),
      allowWaylandKeyboardRemembered: allowWaylandKeyboard,
    )) {
      inputModel.keyboardInputAllowed = false;
      showWaylandKeyboardInputWarningDialog(
        id: widget.id,
        connectionId: sessionId.toString(),
        ffi: gFFI,
        onEnable: () async {
          _openKeyboardUnlocked();
        },
      );
      return;
    }
    _openKeyboardUnlocked();
  }

  void _openKeyboardUnlocked() {
    inputModel.keyboardInputAllowed = true;
    if (inputModel.quest.isQuest.value) {
      unawaited(_openKeyboardQuest());
      return;
    }
    gFFI.invokeMethod("enable_soft_keyboard", true);
    // destroy first, so that our _value trick can work
    // The phone trick pre-fills the hidden field with `initText` ('1' * 1024) so a
    // backspace always has a sentinel char to consume; phone IMEs keep that text
    // silent and only report later edits. The Quest's built-in keyboard instead
    // commits the existing field text back through onChanged when it attaches, so
    // the sentinel '1's were forwarded to the remote and flooded it with "1"s.
    // On a Quest start empty: nothing to echo. (Trade-off: a backspace with an
    // empty local field won't reach the remote; typing is unaffected.)
    _value = initText;
    _textController.text = _value;
    setState(() => _showEdit = false);
    _timer?.cancel();
    _timer = Timer(kMobileDelaySoftKeyboard, () {
      // show now, and sleep a while to requestFocus to
      // make sure edit ready, so that keyboard won't show/hide/show/hide happen
      setState(() => _showEdit = true);
      _timer?.cancel();
      _timer = Timer(kMobileDelaySoftKeyboardFocus, () {
        SystemChrome.setEnabledSystemUIMode(SystemUiMode.manual,
            overlays: SystemUiOverlay.values);
        _mobileFocusNode.requestFocus();
      });
    });
  }

  /// Quest: never tear the hidden field down to (re)open; safe to call repeatedly.
  /// Tearing it down closes the input connection, Flutter hides the IME, and the Quest drops
  /// the show that follows ~30-60 ms later (the flicker).
  Future<void> _openKeyboardQuest() async {
    _timer?.cancel();
    // await: FLAG_ALT_FOCUSABLE_IM cleared before anything asks for the IME.
    await gFFI.invokeMethod("enable_soft_keyboard", true);
    if (!mounted) return;
    if (_showEdit && _mobileFocusNode.hasFocus) {
      // Focused, IME closed by the user (connection kept) -> just show it, like a tapped TextField.
      final ets = _mobileFocusNode.context
          ?.findAncestorStateOfType<EditableTextState>();
      if (ets != null) {
        ets.requestKeyboard(); // re-attaches if the connection is gone, else show()
      } else {
        await SystemChannels.textInput.invokeMethod<void>('TextInput.show');
      }
      if (_textController.text.length > 1024) {
        // bound the 0-px-wide layout
        _value = '';
        _textController.clear();
      }
      return;
    }
    // A new connection follows: clear first, the Quest IME echoes existing text on attach.
    _value = '';
    _textController.clear();
    if (_showEdit) {
      _mobileFocusNode.requestFocus(); // mounted, unfocused: real focus change -> attach + show
      return;
    }
    setState(() => _showEdit = true); // schedules the frame the callback below needs
    WidgetsBinding.instance.addPostFrameCallback((_) {
      if (mounted && _showEdit) _mobileFocusNode.requestFocus();
    });
  }

  /// The collapsed toolbar is a thin strip below the remote image (see
  /// [_collapsedBarHandle]) instead of a button floating over the picture.
  bool get _collapsedHandleShown =>
      !_showBar &&
      !_showGestureHelp &&
      !isWebDesktop &&
      gFFI.ffiModel.pi.displays.isNotEmpty;

  Widget _bottomWidget() {
    final bar = _showGestureHelp
        ? getGestureHelp()
        : (_showBar && gFFI.ffiModel.pi.displays.isNotEmpty
            ? getBottomAppBar()
            : (_collapsedHandleShown ? _collapsedBarHandle() : Offstage()));
    if (!_keysVisible) return bar;
    // The same tree shape whether the row below is open or not: swapping the toolbar for
    // a Column on every arrow tap would rebuild the whole toolbar from scratch.
    return Column(
      mainAxisSize: MainAxisSize.min,
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [bar, if (_keysExpanded || !_arrowFits) _extraKeys()],
    );
  }

  /// Quest: the system keyboard has no Ctrl, Tab, Esc or arrows. The most used of them sit in
  /// the toolbar row itself, after its icons (as many as fit: [_inlineKeys]); a small arrow
  /// next to them opens one more row below the toolbar with the rest and F1..F12
  /// ([_extraKeys]). Nothing is drawn over the remote image.
  bool get _keyBarShown {
    final quest = inputModel.quest;
    return quest.isQuest.value &&
        quest.keyBar.value &&
        !_showGestureHelp &&
        gFFI.ffiModel.pi.displays.isNotEmpty &&
        gFFI.ffiModel.keyboard &&
        !gFFI.ffiModel.viewOnly;
  }

  /// ...and the toolbar that carries the keys is open.
  bool get _keysVisible => _keyBarShown && _showBar;

  bool _keysExpanded = false; // the row below the toolbar is open
  int _inlineKeyCount = 0; // how many of _keySpecs() fitted into the toolbar row
  // The toolbar row has room for at least the arrow. If not (a very narrow window), the
  // row below is shown with every key, so none is lost.
  bool _arrowFits = true;

  static const double _kKeyHeight = 34;
  static const double _kKeyMinWidth = 36;
  static const double _kKeyPadH = 6;
  static const double _kKeyMarginH = 1.5;
  static const double _kKeyFontSize = 12;
  static const double _kKeyIconSize = 18;

  /// All keys, most used first: the toolbar row takes as many as fit from the front.
  List<_KeySpec> _keySpecs() {
    final isMac = gFFI.ffiModel.pi.platform == kPeerPlatformMacOS;
    final isWin = gFFI.ffiModel.pi.platform == kPeerPlatformWindows;
    _KeySpec mod(String label, String name) => _KeySpec(
        label, () => inputModel.cycleModifier(name),
        state: inputModel.modifierState(name));
    _KeySpec k(String label, String vk, {IconData? icon}) =>
        _KeySpec(label, () => inputModel.inputKey(vk), icon: icon);
    _KeySpec combo(String label, String vk) => _KeySpec(
        '${isMac ? 'Cmd' : 'Ctrl'}+$label',
        () => inputModel.inputShortcut(vk, isMac: isMac));
    return [
      mod('Ctrl', 'ctrl'),
      mod('Alt', 'alt'),
      mod('Shift', 'shift'),
      mod(isMac ? 'Cmd' : 'Win', 'command'),
      k('Esc', 'VK_ESCAPE'),
      k('Tab', 'VK_TAB'),
      k('Del', 'VK_DELETE'),
      // Copy, and paste through the Windows clipboard history (Win+V); plain paste for
      // other hosts. They come before the cursor keys: used far more often here.
      combo('C', 'VK_C'),
      if (isWin)
        _KeySpec('Win+V',
            () => inputModel.inputShortcut('VK_V', isMac: false, modifier: 'command'))
      else
        combo('V', 'VK_V'),
      k('Enter', 'VK_ENTER'),
      k('Home', 'VK_HOME'),
      k('End', 'VK_END'),
      k('', 'VK_LEFT', icon: Icons.keyboard_arrow_left),
      k('', 'VK_UP', icon: Icons.keyboard_arrow_up),
      k('', 'VK_DOWN', icon: Icons.keyboard_arrow_down),
      k('', 'VK_RIGHT', icon: Icons.keyboard_arrow_right),
      k('PgUp', 'VK_PRIOR'),
      k('PgDn', 'VK_NEXT'),
      if (isWin) combo('V', 'VK_V'),
      combo('X', 'VK_X'),
      combo('Z', 'VK_Z'),
      combo('A', 'VK_A'),
    ];
  }

  List<_KeySpec> _fnKeySpecs() => [
        for (var i = 1; i <= 12; ++i)
          _KeySpec('F$i', () => inputModel.inputKey('VK_F$i')),
      ];

  TextStyle _keyTextStyle(BuildContext context) =>
      (Theme.of(context).textTheme.bodyMedium ?? const TextStyle())
          .copyWith(color: Colors.white, fontSize: _kKeyFontSize);

  /// Laid-out width of [_keyButton] for [k], margins included.
  double _keyWidth(BuildContext context, _KeySpec k) {
    double inner = _kKeyIconSize;
    if (k.icon == null) {
      final painter = TextPainter(
          text: TextSpan(text: k.label, style: _keyTextStyle(context)),
          textDirection: TextDirection.ltr,
          textScaler: MediaQuery.textScalerOf(context),
          maxLines: 1)
        ..layout();
      inner = painter.width;
      painter.dispose();
    }
    final w = inner + 2 * _kKeyPadH;
    return (w < _kKeyMinWidth ? _kKeyMinWidth : w) + 2 * _kKeyMarginH;
  }

  Widget _keyButton(BuildContext context, _KeySpec k) {
    return Padding(
      padding:
          const EdgeInsets.symmetric(horizontal: _kKeyMarginH, vertical: 2),
      child: Material(
        color: k.state == 2
            ? MyTheme.accent
            : (k.state == 1 ? MyTheme.accent80 : const Color(0xFF3A3A3A)),
        borderRadius: BorderRadius.circular(6),
        child: InkWell(
          borderRadius: BorderRadius.circular(6),
          canRequestFocus: false, // the soft keyboard's text field keeps the focus
          onTap: k.onTap,
          child: Container(
            height: _kKeyHeight,
            constraints: const BoxConstraints(minWidth: _kKeyMinWidth),
            padding: const EdgeInsets.symmetric(horizontal: _kKeyPadH),
            // widthFactor: shrink to the label. A plain alignment would take all the width
            // it is offered (a whole line inside a Wrap).
            child: Center(
              widthFactor: 1.0,
              child: k.icon != null
                  ? Icon(k.icon, size: _kKeyIconSize, color: Colors.white)
                  : Text(k.label,
                      maxLines: 1,
                      softWrap: false,
                      style: _keyTextStyle(context).copyWith(
                          decoration: k.state == 2
                              ? TextDecoration.underline
                              : TextDecoration.none)),
            ),
          ),
        ),
      ),
    );
  }

  /// The keys inside the toolbar row: the first keys of [_keySpecs] that fit into the space
  /// left by the icons, then the arrow that opens [_extraKeys].
  Widget _inlineKeys() {
    return Obx(() {
      inputModel.modifiersRevision.value; // repaint when a one-shot modifier is used up
      return LayoutBuilder(builder: (context, constraints) {
        const leadGap = 8.0;
        void toggle() => setState(() => _keysExpanded = !_keysExpanded);
        // Every icon key has the same width, whatever its icon or state.
        // 1 px of slack per key for rounding in text layout.
        var used = leadGap +
            _keyWidth(context,
                _KeySpec('', toggle, icon: Icons.keyboard_double_arrow_down)) +
            1;
        final fits = used <= constraints.maxWidth;
        final specs = _keySpecs();
        var n = 0;
        if (fits) {
          for (final k in specs) {
            final w = _keyWidth(context, k) + 1;
            if (used + w > constraints.maxWidth) break;
            used += w;
            n++;
          }
        }
        if (n != _inlineKeyCount || fits != _arrowFits) {
          // The row below shows the keys that did not fit here.
          WidgetsBinding.instance.addPostFrameCallback((_) {
            if (mounted && (n != _inlineKeyCount || fits != _arrowFits)) {
              setState(() {
                _inlineKeyCount = n;
                _arrowFits = fits;
              });
            }
          });
        }
        if (!fits) return const SizedBox.shrink();
        // A modifier that is on but sits in the closed row below must still show here.
        final hiddenHeld =
            !_keysExpanded && specs.skip(n).any((k) => k.state != 0);
        // Double chevrons: the single ones are the Up / Down cursor keys in this same row.
        final more = _KeySpec('', toggle,
            icon: _keysExpanded
                ? Icons.keyboard_double_arrow_up
                : Icons.keyboard_double_arrow_down,
            state: _keysExpanded ? 2 : (hiddenHeld ? 1 : 0));
        return Row(
          mainAxisSize: MainAxisSize.max,
          children: [
            const SizedBox(width: leadGap),
            for (final k in specs.take(n)) _keyButton(context, k),
            _keyButton(context, more),
          ],
        );
      });
    });
  }

  /// Toolbar buttons between the keys and the collapse arrow (Quest): the overview of the
  /// displays, "-" (the display added last goes) and "+" (one more display in a window of
  /// its own). See AndroidDisplayPanels.
  List<Widget> _displayButtons(BuildContext context) {
    if (_isPanel) return const [];
    final ffiModel = gFFI.ffiModel;
    final pi = ffiModel.pi;
    if (!inputModel.quest.isQuest.value ||
        _showGestureHelp ||
        pi.displays.isEmpty) {
      return const [];
    }
    final privacyModeState = PrivacyModeState.find(widget.id);
    if (privacyModeState.isNotEmpty) return const [];
    final panels = androidDisplayPanels;
    return [
      const SizedBox(width: 4),
      const _QuestStatusChip(),
      Obx(() => _keyButton(
          context,
          _KeySpec('', _showDisplayOverview,
              icon: Icons.grid_view, state: panels.isOpenRx.value ? 1 : 0))),
      _keyButton(
          context,
          _KeySpec('', () => panels.removeLast(gFFI), icon: Icons.remove)),
      _keyButton(
          context, _KeySpec('', () => panels.addOne(gFFI), icon: Icons.add)),
    ];
  }

  /// The displays in the host's left-to-right order: move one, give it a window, or take
  /// it away.
  void _showDisplayOverview() {
    final panels = androidDisplayPanels;
    gFFI.dialogManager.show((setState, close, context) {
      return CustomAlertDialog(
        title: const Text('Kijelzők'),
        content: Obx(() {
          panels.revision.value;
          final pi = gFFI.ffiModel.pi;
          pi.displays.length; // rebuild when the host's list or positions change
          final order = panels.order(gFFI);
          final keep = panels.mainDisplay(gFFI);
          return Column(
            mainAxisSize: MainAxisSize.min,
            children: [
              SingleChildScrollView(
                scrollDirection: Axis.horizontal,
                child: Row(
                  mainAxisSize: MainAxisSize.min,
                  children: [
                    for (var k = 0; k < order.length; k++)
                      _displayTile(context, order[k], k, order.length, keep),
                  ],
                ),
              ),
              const SizedBox(height: 6),
              const Text(
                  'Balról jobbra, ahogy a Windows látja őket. '
                  'A nyilakkal a sorrend állítható.',
                  style: TextStyle(fontSize: 12)),
              Row(
                mainAxisSize: MainAxisSize.min,
                children: [
                  Checkbox(
                      value: panels.autoOrder,
                      onChanged: (v) => panels.autoOrder = v == true),
                  const Flexible(
                      child: Text(
                          'A Windows kövesse az ablakok térbeli elrendezését',
                          style: TextStyle(fontSize: 12))),
                ],
              ),
              // Hardware decoding into a texture (enable-android-surface-decode): on
              // unless "N"; read when a connection starts.
              Row(
                mainAxisSize: MainAxisSize.min,
                children: [
                  Checkbox(
                      value: bind.mainGetLocalOption(
                              key: kOptionEnableAndroidSurfaceDecode) !=
                          'N',
                      onChanged: (v) async {
                        await bind.mainSetLocalOption(
                            key: kOptionEnableAndroidSurfaceDecode,
                            value: v == true ? 'Y' : 'N');
                        setState(() {});
                      }),
                  const Flexible(
                      child: Text('Hardveres dekódolás (kísérleti)',
                          style: TextStyle(fontSize: 12))),
                ],
              ),
              const Text('A hardveres dekódolás a következő csatlakozástól érvényes.',
                  style: TextStyle(fontSize: 11)),
            ],
          );
        }),
        actions: [dialogButton('Close', onPressed: close)],
        onCancel: close,
      );
    });
  }

  Widget _displayTile(
      BuildContext context, int d, int pos, int count, int keep) {
    final panels = androidDisplayPanels;
    final pi = gFFI.ffiModel.pi;
    if (d < 0 || d >= pi.displays.length) return const SizedBox.shrink();
    final disp = pi.displays[d];
    final isMain = d == keep;
    final open = isMain || panels.isPanelOpen(d);
    // Amyuni: only the last display can go; RustDesk IDD: any virtual one but the main.
    final removes = panels.removesDisplay(gFFI, d);
    Widget arrow(IconData icon, int by, bool enabled) => InkWell(
        onTap: enabled ? () => panels.move(gFFI, d, by) : null,
        child: Padding(
            padding: const EdgeInsets.all(4),
            child: Icon(icon,
                size: 22,
                color: enabled ? null : Theme.of(context).disabledColor)));
    return Container(
      width: 104,
      margin: const EdgeInsets.all(3),
      padding: const EdgeInsets.symmetric(vertical: 6, horizontal: 4),
      decoration: BoxDecoration(
          border: Border.all(color: Theme.of(context).hintColor),
          borderRadius: BorderRadius.circular(6),
          color: open ? MyTheme.accent.withOpacity(0.25) : null),
      child: Column(
        mainAxisSize: MainAxisSize.min,
        children: [
          Text('${d + 1}',
              style:
                  const TextStyle(fontSize: 20, fontWeight: FontWeight.bold)),
          Text('${disp.width}×${disp.height}',
              style: const TextStyle(fontSize: 11)),
          Text(isMain ? 'fő ablak' : (open ? 'saját ablak' : 'nincs ablaka'),
              style: const TextStyle(fontSize: 11)),
          Row(
            mainAxisAlignment: MainAxisAlignment.center,
            children: [
              arrow(Icons.chevron_left, -1, pos > 0),
              arrow(Icons.chevron_right, 1, pos < count - 1),
            ],
          ),
          if (!isMain)
            TextButton(
              onPressed: () {
                if (open) {
                  panels.removeDisplay(gFFI, d);
                } else {
                  panels.openPanel(gFFI, d);
                }
              },
              child: Text(open ? (removes ? 'Eltávolít' : 'Bezár') : 'Megnyit',
                  style: const TextStyle(fontSize: 12)),
            )
          else
            const SizedBox(height: 40),
        ],
      ),
    );
  }

  /// The row that opens below the toolbar: the keys that did not fit into it, then F1..F12.
  Widget _extraKeys() {
    return Material(
      color: _kBottomBarColor,
      child: Padding(
        padding: const EdgeInsets.fromLTRB(8, 0, 8, 5),
        child: Obx(() {
          inputModel.modifiersRevision.value;
          return Builder(builder: (context) {
            return Wrap(
              alignment: WrapAlignment.center,
              crossAxisAlignment: WrapCrossAlignment.center,
              children: [
                for (final k in _keySpecs().skip(_inlineKeyCount))
                  _keyButton(context, k),
                const SizedBox(width: 8),
                for (final k in _fnKeySpecs()) _keyButton(context, k),
              ],
            );
          });
        }),
      ),
    );
  }

  static const double _kCollapsedBarHeight = 28;

  /// The toolbar sits right under the remote desktop, so it is dark like a Windows taskbar
  /// in dark mode instead of the accent blue.
  static const Color _kBottomBarColor = Color(0xFF1C1C1C);
  static const Color _kBottomBarHandleColor = Color(0xFF3A3A3A);

  Widget _collapsedBarHandle() {
    return Material(
      color: _kBottomBarColor,
      child: SizedBox(
        height: _kCollapsedBarHeight,
        child: Align(
          alignment: Alignment.centerRight,
          child: Padding(
            padding: const EdgeInsets.only(right: 16),
            // Not gated on waitForFirstImage: during a slow reconnect this handle is the
            // only way back to the toolbar (close, display, actions).
            child: InkWell(
              onTap: () => setState(() => _showBar = true),
              child: Container(
                width: 72,
                height: _kCollapsedBarHeight,
                decoration: const BoxDecoration(
                  color: _kBottomBarHandleColor,
                  borderRadius: BorderRadius.vertical(top: Radius.circular(10)),
                ),
                child:
                    const Icon(Icons.expand_less, color: Colors.white, size: 22),
              ),
            ),
          ),
        ),
      ),
    );
  }

  /// The body is laid out above the bottom bar; tell the canvas how tall the bar is so the
  /// remote image is fitted into what is left, not into the whole window.
  void _onBottomBarHeight(double height) {
    if (!mounted) return;
    final canvas = gFFI.canvasModel;
    if ((canvas.mobileBottomBarHeight - height).abs() < 0.5) return;
    canvas.setMobileBottomBarHeight(height);
  }

  @override
  Widget build(BuildContext context) {
    final keyboardIsVisible =
        keyboardVisibilityController.isVisible && _showEdit;
    final showActionButton = keyboardIsVisible ||
        _showGestureHelp ||
        (!_showBar && !_collapsedHandleShown);

    return WillPopScope(
      onWillPop: () async {
        if (_isPanel) {
          closeDisplayPanel();
        } else {
          clientClose(sessionId, gFFI);
        }
        return false;
      },
      child: Scaffold(
          // workaround for https://github.com/rustdesk/rustdesk/issues/3131
          floatingActionButtonLocation: keyboardIsVisible
              ? FABLocation(FloatingActionButtonLocation.endFloat, 0, -35)
              : null,
          floatingActionButton: !showActionButton
              ? null
              : FloatingActionButton(
                  mini: !keyboardIsVisible,
                  child: Icon(
                    (keyboardIsVisible || _showGestureHelp)
                        ? Icons.expand_more
                        : Icons.expand_less,
                    color: Colors.white,
                  ),
                  backgroundColor: MyTheme.accent,
                  onPressed: () {
                    setState(() {
                      if (keyboardIsVisible) {
                        _showEdit = false;
                        gFFI.invokeMethod("enable_soft_keyboard", false);
                        _mobileFocusNode.unfocus();
                        _physicalFocusNode.requestFocus();
                      } else if (_showGestureHelp) {
                        _showGestureHelp = false;
                      } else {
                        _showBar = !_showBar;
                      }
                    });
                  }),
          bottomNavigationBar: Obx(() => Stack(
                alignment: Alignment.bottomCenter,
                children: [
                  gFFI.ffiModel.pi.isSet.isTrue &&
                          gFFI.ffiModel.waitForFirstImage.isTrue
                      ? emptyOverlay(MyTheme.canvasColor)
                      : () {
                          gFFI.ffiModel.tryShowAndroidActionsOverlay();
                          return Offstage();
                        }(),
                  _HeightReporter(
                      onHeight: _onBottomBarHeight, child: _bottomWidget()),
                  gFFI.ffiModel.pi.isSet.isFalse
                      ? emptyOverlay(MyTheme.canvasColor)
                      : Offstage(),
                ],
              )),
          body: Obx(
            () => getRawPointerAndKeyBody(Overlay(
              initialEntries: [
                OverlayEntry(builder: (context) {
                  return Container(
                    color: kColorCanvas,
                    child: isWebDesktop
                        ? getBodyForDesktopWithListener()
                        : SafeArea(
                            child:
                                OrientationBuilder(builder: (ctx, orientation) {
                              if (_currentOrientation != orientation) {
                                Timer(const Duration(milliseconds: 200), () {
                                  gFFI.dialogManager
                                      .resetMobileActionsOverlay(ffi: gFFI);
                                  _currentOrientation = orientation;
                                  gFFI.canvasModel.updateViewStyle();
                                });
                              }
                              // Quest pointer mode: the laser and direct touch are absolute
                              // pointers handled by InputModel, not phone gestures.
                              return Container(
                                color: MyTheme.canvasColor,
                                child: Obx(() => inputModel
                                            .isPhysicalMouse.value ||
                                        inputModel.quest.pointerModeActive
                                    ? getBodyForMobile()
                                    : RawTouchGestureDetectorRegion(
                                        child: getBodyForMobile(),
                                        ffi: gFFI,
                                      )),
                              );
                            }),
                          ),
                  );
                })
              ],
            )),
          )),
    );
  }

  Widget getRawPointerAndKeyBody(Widget child) {
    final ffiModel = Provider.of<FfiModel>(context);
    return RawPointerMouseRegion(
      // Upstream hides the system pointer over the remote image (it draws the remote cursor
      // itself). Never on a Quest: its own pointer is the only one the user aims with, and
      // this app must not ask the system to take it away.
      cursor: ffiModel.keyboard && !inputModel.quest.isQuest.value
          ? SystemMouseCursors.none
          : MouseCursor.defer,
      inputModel: inputModel,
      // Disable RawKeyFocusScope before the connecting is established.
      // The "Delete" key on the soft keyboard may be grabbed when inputting the password dialog.
      child: gFFI.ffiModel.pi.isSet.isTrue
          ? RawKeyFocusScope(
              focusNode: _physicalFocusNode,
              inputModel: inputModel,
              child: child)
          : child,
    );
  }

  Widget getBottomAppBar() {
    final ffiModel = Provider.of<FfiModel>(context);
    return BottomAppBar(
      elevation: 10,
      color: _kBottomBarColor,
      child: Row(
        mainAxisSize: MainAxisSize.max,
        mainAxisAlignment: MainAxisAlignment.spaceBetween,
        children: <Widget>[
          Row(
              children: <Widget>[
                    IconButton(
                      color: Colors.white,
                      icon: Icon(Icons.clear),
                      onPressed: () {
                        if (_isPanel) {
          closeDisplayPanel();
        } else {
          clientClose(sessionId, gFFI);
        }
                      },
                    ),
                    if (!_isPanel) IconButton(
                      color: Colors.white,
                      icon: Icon(Icons.tv),
                      onPressed: () {
                        setState(() => _showEdit = false);
                        showOptions(context, widget.id, gFFI.dialogManager);
                      },
                    )
                  ] +
                  (isWebDesktop || ffiModel.viewOnly || !ffiModel.keyboard
                      ? []
                      : gFFI.ffiModel.isPeerAndroid
                          ? [
                              IconButton(
                                  color: Colors.white,
                                  icon: Icon(Icons.keyboard),
                                  onPressed: openKeyboard),
                              IconButton(
                                color: Colors.white,
                                icon: const Icon(Icons.build),
                                onPressed: () => gFFI.dialogManager
                                    .toggleMobileActionsOverlay(ffi: gFFI),
                              )
                            ]
                          : [
                              IconButton(
                                  color: Colors.white,
                                  icon: Icon(Icons.keyboard),
                                  onPressed: openKeyboard),
                              IconButton(
                                color: Colors.white,
                                icon: Icon(gFFI.ffiModel.touchMode
                                    ? Icons.touch_app
                                    : Icons.mouse),
                                onPressed: () => setState(
                                    () => _showGestureHelp = !_showGestureHelp),
                              ),
                            ]) +
                  (isWeb || _isPanel
                      ? []
                      : <Widget>[
                          futureBuilder(
                              future: gFFI.invokeMethod(
                                  "get_value", "KEY_IS_SUPPORT_VOICE_CALL"),
                              hasData: (isSupportVoiceCall) => IconButton(
                                    color: Colors.white,
                                    icon: isAndroid && isSupportVoiceCall
                                        ? SvgPicture.asset('assets/chat.svg',
                                            colorFilter: ColorFilter.mode(
                                                Colors.white, BlendMode.srcIn))
                                        : Icon(Icons.message),
                                    onPressed: () =>
                                        isAndroid && isSupportVoiceCall
                                            ? showChatOptions(widget.id)
                                            : onPressedTextChat(widget.id),
                                  ))
                        ]) +
                  [
                    if (_isPanel)
                      Padding(
                          padding: const EdgeInsets.symmetric(horizontal: 10),
                          child: Text('${widget.panelDisplay! + 1}',
                              style: const TextStyle(
                                  color: Colors.white,
                                  fontWeight: FontWeight.bold))),
                    if (isAndroid && !_isPanel) _questMenuButton(),
                    if (isAndroid && !_isPanel) Obx(() {
                      final model = gFFI.ffiModel;
                      final active = model.microphoneForwardingState.value == 'active';
                      final pending = ['pending', 'capturing'].contains(model.microphoneForwardingState.value);
                      return IconButton(
                        tooltip: model.microphoneForwardingError.value.isNotEmpty
                            ? model.microphoneForwardingError.value
                            : translate(active ? 'Stop forwarding microphone' : 'Forward microphone'),
                        color: active ? Colors.redAccent : pending ? Colors.orange : Colors.white,
                        icon: Icon(active ? Icons.mic : Icons.mic_off),
                        onPressed: model.toggleMicrophoneForwarding,
                      );
                    }),
                    if (!_isPanel) IconButton(
                      color: Colors.white,
                      icon: Icon(Icons.more_vert),
                      onPressed: () {
                        setState(() => _showEdit = false);
                        showActions(widget.id);
                      },
                    ),
                  ]),
          if (_keysVisible) Expanded(child: _inlineKeys()),
          ..._displayButtons(context),
          Obx(() => IconButton(
                color: Colors.white,
                icon: Icon(Icons.expand_more),
                onPressed: gFFI.ffiModel.waitForFirstImage.isTrue
                    ? null
                    : () {
                        // The keys go away with the toolbar: do not leave a modifier
                        // held with nothing on screen to show or release it.
                        if (_keyBarShown) inputModel.resetModifiers();
                        setState(() => _showBar = !_showBar);
                      },
              )),
        ],
      ),
    );
  }

  /// Raw controller/pointer events received on a Quest, so the user can report what arrives.
  /// IgnorePointer: the laser must reach the remote screen underneath.
  Widget _questDebugReadout() {
    final quest = inputModel.quest;
    return Positioned(
      left: 8,
      top: 8,
      child: IgnorePointer(
        child: Obx(() {
          if (!quest.isQuest.value || !quest.debug.value) {
            return const SizedBox.shrink();
          }
          final lines = <String>[
            'Quest input | pointer mode ${quest.pointerMode.value ? 'ON' : 'OFF'}'
                '${quest.debugStatus.value.isEmpty ? '' : ' | ${quest.debugStatus.value}'}',
            ...quest.debugLines,
          ];
          return Container(
            constraints: const BoxConstraints(maxWidth: 620),
            padding: const EdgeInsets.symmetric(horizontal: 6, vertical: 4),
            color: Colors.black.withOpacity(0.6),
            child: Text(
              lines.join('\n'),
              style: const TextStyle(
                  color: Colors.greenAccent,
                  fontSize: 11,
                  fontFamily: 'monospace',
                  height: 1.25),
            ),
          );
        }),
      ),
    );
  }

  /// Quest immersive game mode (XrActivity, phase 1: a black immersive view, the controllers
  /// only logged). The main view shows a single display first; this page and its session stay
  /// alive behind the immersive view, and the panels the system closes meanwhile come back
  /// when it ends (AndroidDisplayPanels.gameModeSuspended).
  Future<void> _enterGameMode() async {
    final ffi = gFFI;
    final pi = ffi.ffiModel.pi;
    if (pi.displays.isEmpty || ffi.ffiModel.waitForFirstImage.isTrue) return;
    // Asked before the switch: nothing of this page is visible in the immersive view, so the
    // way out (and the warning about the system menu's Quit, risk 10) must be read here.
    final go = await showDialog<bool>(
      context: context,
      builder: (ctx) => AlertDialog(
        title: const Text('Játék mód'),
        content: const Text(
            'Kilépés: tartsd nyomva a bal Menü gombot 1,5 mp-ig (vagy Menü+L3+R3).\n\n'
            'A rendszermenü kilépés gombja az egész appot bezárhatja, a kapcsolattal együtt.'),
        actions: [
          TextButton(
              onPressed: () => Navigator.of(ctx).pop(false),
              child: const Text('Mégse')),
          TextButton(
              onPressed: () => Navigator.of(ctx).pop(true),
              child: const Text('Belépés')),
        ],
      ),
    );
    if (go != true || !mounted || ffi.closed) return;
    final panels = androidDisplayPanels;
    final d = await panels.enterGameMode(ffi);
    if (d < 0 || d >= pi.displays.length) {
      await panels.onGameModeExited(ffi, 'no display');
      return;
    }
    final disp = pi.displays[d];
    bool ok = false;
    try {
      ok = await platformFFI.invokeMethodWithResult<bool>('game_mode_enter', {
            'sid': ffi.sessionId.toString(),
            'display': d,
            'w': disp.width,
            'h': disp.height,
            // Features.gamepad comes with the host pad (phase 3); 0 = immersive view only.
            'pad': 0,
          }) ==
          true;
    } catch (e) {
      debugPrint('[gamemode] enter failed: $e');
    }
    if (!ok) {
      await panels.onGameModeExited(ffi, 'not started');
      showToast('A játék mód nem indult el');
    }
  }

  Widget _questMenuButton() {
    final quest = inputModel.quest;
    return Obx(() {
      if (!quest.isQuest.value) return const SizedBox.shrink();
      return PopupMenuButton<String>(
        icon: const Icon(Icons.sports_esports, color: Colors.white),
        tooltip: 'Quest controller',
        onSelected: (v) {
          if (v == 'gamepad') {
            unawaited(quest.setGamepadMode(
                !quest.gamepadCapture.value, gFFI.sessionId.toString()));
          }
          if (v == 'gamemode') _enterGameMode();
          if (v == 'pointer') quest.setPointerMode(!quest.pointerMode.value);
          if (v == 'cursor') quest.setCursorOverlay(!quest.cursorOverlay.value);
          if (v == 'autokbd') {
            quest.setAutoKeyboard(!quest.autoKeyboard.value);
          }
          if (v == 'debug') quest.setDebug(!quest.debug.value);
          if (v == 'keybar') quest.setKeyBar(!quest.keyBar.value);
          if (v == 'dragscroll') {
            inputModel.setDragScroll(!inputModel.dragScroll.value);
          }
          if (v == 'smoothscroll') {
            inputModel.setDragScrollSmooth(!inputModel.dragScrollSmooth.value);
          }
        },
        itemBuilder: (_) => [
          CheckedPopupMenuItem(
            value: 'gamepad',
            checked: quest.gamepadCapture.value,
            child: const Text('Gamepad mód (Xbox-kontroller a gépen)'),
          ),
          PopupMenuItem(
            value: 'gamemode',
            enabled: gFFI.ffiModel.pi.displays.isNotEmpty &&
                gFFI.ffiModel.waitForFirstImage.isFalse,
            child: const Text('Játék mód'),
          ),
          CheckedPopupMenuItem(
            value: 'pointer',
            checked: quest.pointerMode.value,
            child: const Text('Quest pointer mode (laser/touch = mouse)'),
          ),
          CheckedPopupMenuItem(
            value: 'cursor',
            checked: quest.cursorOverlay.value,
            child: const Text('Show cursor overlay'),
          ),
          CheckedPopupMenuItem(
            value: 'autokbd',
            checked: quest.autoKeyboard.value,
            child: const Text('Auto-open keyboard on text fields'),
          ),
          CheckedPopupMenuItem(
            value: 'keybar',
            checked: quest.keyBar.value,
            child: const Text('Extra keys in the toolbar (Ctrl, Tab, Esc...)'),
          ),
          CheckedPopupMenuItem(
            value: 'dragscroll',
            checked: inputModel.dragScroll.value,
            child: const Text('Vertical drag scrolls (sideways drag selects)'),
          ),
          CheckedPopupMenuItem(
            value: 'smoothscroll',
            checked: inputModel.dragScrollSmooth.value,
            child: const Text('Smooth scrolling (off: wheel notches)'),
          ),
          CheckedPopupMenuItem(
            value: 'debug',
            checked: quest.debug.value,
            child: const Text('Show input debug readout'),
          ),
        ],
      );
    });
  }

  bool get showCursorPaint =>
      !gFFI.ffiModel.isPeerAndroid &&
      !gFFI.canvasModel.cursorEmbedded &&
      !gFFI.inputModel.relativeMouseMode.value;

  Widget getBodyForMobile() {
    final keyboardIsVisible = keyboardVisibilityController.isVisible;
    return Container(
        color: MyTheme.canvasColor,
        child: Stack(children: () {
          final paints = [
            ImagePaint(ffiModel: gFFI.ffiModel),
            Positioned(
              top: 10,
              right: 10,
              child: QualityMonitor(gFFI.qualityMonitorModel),
            ),
            KeyHelpTools(
                keyboardIsVisible: keyboardIsVisible,
                showGestureHelp: _showGestureHelp,
                keyBarShown: _keysVisible),
            SizedBox(
              width: 0,
              height: 0,
              child: !_showEdit
                  ? Container()
                  : TextFormField(
                      textInputAction: TextInputAction.newline,
                      autocorrect: false,
                      // Flutter 3.16.9 Android.
                      // `enableSuggestions` causes secure keyboard to be shown.
                      // https://github.com/flutter/flutter/issues/139143
                      // https://github.com/flutter/flutter/issues/146540
                      // enableSuggestions: false,
                      autofocus: true,
                      focusNode: _mobileFocusNode,
                      // Quest laser clicks arrive as mouse PointerDowns; Flutter's Android
                      // default unfocuses on those, closing the keyboard on every click.
                      onTapOutside: (e) {
                        if (inputModel.quest.isQuest.value) return;
                        if (e.kind != PointerDeviceKind.touch) {
                          _mobileFocusNode.unfocus();
                        }
                      },
                      maxLines: null,
                      controller: _textController,
                      // trick way to make backspace work always
                      keyboardType: TextInputType.multiline,
                      // `onChanged` may be called depending on the input method if this widget is wrapped in
                      // `Focus(onKeyEvent: ..., child: ...)`
                      // For `Backspace` button in the soft keyboard:
                      // en/fr input method:
                      //      1. The button will not trigger `onKeyEvent` if the text field is not empty.
                      //      2. The button will trigger `onKeyEvent` if the text field is empty.
                      // ko/zh/ja input method: the button will trigger `onKeyEvent`
                      //                     and the event will not popup if `KeyEventResult.handled` is returned.
                      onChanged: handleSoftKeyboardInput,
                    ).workaroundFreezeLinuxMint(),
            ),
          ];
          if (showCursorPaint) {
            // On a Quest the cursor overlay is toggleable (default on); elsewhere always shown.
            paints.add(Obx(() =>
                (!inputModel.quest.isQuest.value ||
                        inputModel.quest.cursorOverlay.value)
                    ? CursorPaint(widget.id)
                    : const SizedBox.shrink()));
          }
          paints.add(_questDebugReadout());
          if (gFFI.ffiModel.touchMode) {
            paints.add(FloatingMouse(
              ffi: gFFI,
            ));
          } else {
            paints.add(FloatingMouseWidgets(
              ffi: gFFI,
            ));
          }
          return paints;
        }()));
  }

  Widget getBodyForDesktopWithListener() {
    final ffiModel = Provider.of<FfiModel>(context);
    var paints = <Widget>[ImagePaint(ffiModel: ffiModel)];
    if (showCursorPaint) {
      final cursor = bind.sessionGetToggleOptionSync(
          sessionId: sessionId, arg: 'show-remote-cursor');
      if (ffiModel.keyboard || cursor) {
        paints.add(CursorPaint(widget.id));
      }
    }
    return Container(
        color: MyTheme.canvasColor, child: Stack(children: paints));
  }

  List<TTextMenu> _getMobileActionMenus() {
    if (gFFI.ffiModel.pi.platform != kPeerPlatformAndroid ||
        !gFFI.ffiModel.keyboard) {
      return [];
    }
    final enabled = versionCmp(gFFI.ffiModel.pi.version, '1.2.7') >= 0;
    if (!enabled) return [];
    return [
      TTextMenu(
        child: Text(translate('Back')),
        onPressed: () => gFFI.inputModel.onMobileBack(),
      ),
      TTextMenu(
        child: Text(translate('Home')),
        onPressed: () => gFFI.inputModel.onMobileHome(),
      ),
      TTextMenu(
        child: Text(translate('Apps')),
        onPressed: () => gFFI.inputModel.onMobileApps(),
      ),
      TTextMenu(
        child: Text(translate('Volume up')),
        onPressed: () => gFFI.inputModel.onMobileVolumeUp(),
      ),
      TTextMenu(
        child: Text(translate('Volume down')),
        onPressed: () => gFFI.inputModel.onMobileVolumeDown(),
      ),
      TTextMenu(
        child: Text(translate('Power')),
        onPressed: () => gFFI.inputModel.onMobilePower(),
      ),
    ];
  }

  void showActions(String id) async {
    final size = MediaQuery.of(context).size;
    final x = 120.0;
    final y = size.height;
    final mobileActionMenus = _getMobileActionMenus();
    final menus = toolbarControls(context, id, gFFI);

    final List<PopupMenuEntry<int>> more = [
      ...mobileActionMenus
          .asMap()
          .entries
          .map((e) =>
              PopupMenuItem<int>(child: e.value.getChild(), value: e.key))
          .toList(),
      if (mobileActionMenus.isNotEmpty) PopupMenuDivider(),
      ...menus
          .asMap()
          .entries
          .map((e) => PopupMenuItem<int>(
              child: e.value.getChild(),
              value: e.key + mobileActionMenus.length))
          .toList(),
    ];
    () async {
      var index = await showMenu(
        context: context,
        position: RelativeRect.fromLTRB(x, y, x, y),
        items: more,
        elevation: 8,
      );
      if (index != null) {
        if (index < mobileActionMenus.length) {
          mobileActionMenus[index].onPressed?.call();
        } else if (index < mobileActionMenus.length + more.length) {
          menus[index - mobileActionMenus.length].onPressed?.call();
        }
      }
    }();
  }

  onPressedTextChat(String id) {
    gFFI.chatModel.changeCurrentKey(MessageKey(id, ChatModel.clientModeID));
    gFFI.chatModel.toggleChatOverlay();
  }

  showChatOptions(String id) async {
    onPressVoiceCall() => bind.sessionRequestVoiceCall(sessionId: sessionId);
    onPressEndVoiceCall() => bind.sessionCloseVoiceCall(sessionId: sessionId);

    makeTextMenu(String label, Widget icon, VoidCallback onPressed,
            {TextStyle? labelStyle}) =>
        TTextMenu(
          child: Text(translate(label), style: labelStyle),
          trailingIcon: Transform.scale(
            scale: (isDesktop || isWebDesktop) ? 0.8 : 1,
            child: IgnorePointer(
              child: IconButton(
                onPressed: null,
                icon: icon,
              ),
            ),
          ),
          onPressed: onPressed,
        );

    final isInVoice = [
      VoiceCallStatus.waitingForResponse,
      VoiceCallStatus.connected
    ].contains(gFFI.chatModel.voiceCallStatus.value);
    final menus = [
      if (isAndroid)
        makeTextMenu(
          ['active', 'pending', 'capturing'].contains(gFFI.ffiModel.microphoneForwardingState.value)
              ? 'Stop forwarding microphone' : 'Forward microphone',
          Icon(Icons.mic, color: gFFI.ffiModel.microphoneForwardingState.value == 'active'
              ? Colors.redAccent : MyTheme.accent),
          () => gFFI.ffiModel.toggleMicrophoneForwarding()),
      makeTextMenu('Text chat', Icon(Icons.message, color: MyTheme.accent),
          () => onPressedTextChat(widget.id)),
      isInVoice
          ? makeTextMenu(
              'End voice call',
              SvgPicture.asset(
                'assets/call_wait.svg',
                colorFilter:
                    ColorFilter.mode(Colors.redAccent, BlendMode.srcIn),
              ),
              onPressEndVoiceCall,
              labelStyle: TextStyle(color: Colors.redAccent))
          : makeTextMenu(
              'Voice call',
              SvgPicture.asset(
                'assets/call_wait.svg',
                colorFilter: ColorFilter.mode(MyTheme.accent, BlendMode.srcIn),
              ),
              onPressVoiceCall),
    ];

    final menuItems = menus
        .asMap()
        .entries
        .map((e) => PopupMenuItem<int>(child: e.value.getChild(), value: e.key))
        .toList();
    Future.delayed(Duration.zero, () async {
      final size = MediaQuery.of(context).size;
      final x = 120.0;
      final y = size.height;
      var index = await showMenu(
        context: context,
        position: RelativeRect.fromLTRB(x, y, x, y),
        items: menuItems,
        elevation: 8,
      );
      if (index != null && index < menus.length) {
        menus[index].onPressed?.call();
      }
    });
  }

  /// aka changeTouchMode
  BottomAppBar getGestureHelp() {
    return BottomAppBar(
        child: SingleChildScrollView(
            controller: ScrollController(),
            padding: EdgeInsets.symmetric(vertical: 10),
            child: GestureHelp(
              touchMode: gFFI.ffiModel.touchMode,
              onTouchModeChange: (t) {
                gFFI.ffiModel.toggleTouchMode();
                final v = gFFI.ffiModel.touchMode ? 'Y' : 'N';
                bind.mainSetLocalOption(key: kOptionTouchMode, value: v);
              },
              virtualMouseMode: gFFI.ffiModel.virtualMouseMode,
              inputModel: gFFI.inputModel,
            )));
  }

  // * Currently mobile does not enable map mode
  // void changePhysicalKeyboardInputMode() async {
  //   var current = await bind.sessionGetKeyboardMode(id: widget.id) ?? "legacy";
  //   gFFI.dialogManager.show((setState, close) {
  //     void setMode(String? v) async {
  //       await bind.sessionSetKeyboardMode(id: widget.id, value: v ?? "");
  //       setState(() => current = v ?? '');
  //       Future.delayed(Duration(milliseconds: 300), close);
  //     }
  //
  //     return CustomAlertDialog(
  //         title: Text(translate('Physical Keyboard Input Mode')),
  //         content: Column(mainAxisSize: MainAxisSize.min, children: [
  //           getRadio('Legacy mode', 'legacy', current, setMode),
  //           getRadio('Map mode', 'map', current, setMode),
  //         ]));
  //   }, clickMaskDismiss: true);
  // }
}

class KeyHelpTools extends StatefulWidget {
  final bool keyboardIsVisible;
  final bool showGestureHelp;

  /// The Quest's keys in the toolbar are shown: they replace this panel, and only one of the
  /// two may drive the modifier flags.
  final bool keyBarShown;

  /// need to show by external request, etc [keyboardIsVisible] or [changeTouchMode]
  bool get requestShow => keyboardIsVisible || showGestureHelp;

  KeyHelpTools(
      {required this.keyboardIsVisible,
      required this.showGestureHelp,
      this.keyBarShown = false});

  @override
  State<KeyHelpTools> createState() => _KeyHelpToolsState();
}

class _KeyHelpToolsState extends State<KeyHelpTools> {
  var _more = true;
  var _fn = false;
  var _pin = false;
  final _keyboardVisibilityController = KeyboardVisibilityController();
  final _key = GlobalKey();

  InputModel get inputModel => gFFI.inputModel;

  Widget wrap(String text, void Function() onPressed,
      {bool? active, IconData? icon}) {
    return TextButton(
        style: TextButton.styleFrom(
          minimumSize: Size(0, 0),
          padding: EdgeInsets.symmetric(vertical: 10, horizontal: 9.75),
          //adds padding inside the button
          tapTargetSize: MaterialTapTargetSize.shrinkWrap,
          //limits the touch area to the button area
          shape: RoundedRectangleBorder(
            borderRadius: BorderRadius.circular(5.0),
          ),
          backgroundColor: active == true ? MyTheme.accent80 : null,
        ),
        child: icon != null
            ? Icon(icon, size: 14, color: Colors.white)
            : Text(translate(text),
                style: TextStyle(color: Colors.white, fontSize: 11)),
        onPressed: onPressed);
  }

  _updateRect() {
    RenderObject? renderObject = _key.currentContext?.findRenderObject();
    if (renderObject == null) {
      return;
    }
    if (renderObject is RenderBox) {
      final size = renderObject.size;
      Offset pos = renderObject.localToGlobal(Offset.zero);
      gFFI.cursorModel.keyHelpToolsVisibilityChanged(
          Rect.fromLTWH(pos.dx, pos.dy, size.width, size.height),
          widget.keyboardIsVisible);
    }
  }

  @override
  Widget build(BuildContext context) {
    final hasModifierOn = inputModel.ctrl ||
        inputModel.alt ||
        inputModel.shift ||
        inputModel.command;

    if (widget.keyBarShown ||
        (!_pin && !hasModifierOn && !widget.requestShow)) {
      gFFI.cursorModel
          .keyHelpToolsVisibilityChanged(null, widget.keyboardIsVisible);
      return Offstage();
    }
    final size = MediaQuery.of(context).size;

    final pi = gFFI.ffiModel.pi;
    final isMac = pi.platform == kPeerPlatformMacOS;
    final isWin = pi.platform == kPeerPlatformWindows;
    final isLinux = pi.platform == kPeerPlatformLinux;
    final modifiers = <Widget>[
      wrap('Ctrl ', () {
        setState(() => inputModel.ctrl = !inputModel.ctrl);
      }, active: inputModel.ctrl),
      wrap(' Alt ', () {
        setState(() => inputModel.alt = !inputModel.alt);
      }, active: inputModel.alt),
      wrap('Shift', () {
        setState(() => inputModel.shift = !inputModel.shift);
      }, active: inputModel.shift),
      wrap(isMac ? ' Cmd ' : ' Win ', () {
        setState(() => inputModel.command = !inputModel.command);
      }, active: inputModel.command),
    ];
    final keys = <Widget>[
      wrap(
          ' Fn ',
          () => setState(
                () {
                  _fn = !_fn;
                  if (_fn) {
                    _more = false;
                  }
                },
              ),
          active: _fn),
      wrap(
          '',
          () => setState(
                () => _pin = !_pin,
              ),
          active: _pin,
          icon: Icons.push_pin),
      wrap(
          ' ... ',
          () => setState(
                () {
                  _more = !_more;
                  if (_more) {
                    _fn = false;
                  }
                },
              ),
          active: _more),
    ];
    final fn = <Widget>[
      SizedBox(width: 9999),
    ];
    for (var i = 1; i <= 12; ++i) {
      final name = 'F$i';
      fn.add(wrap(name, () {
        inputModel.inputKey('VK_$name');
      }));
    }
    final more = <Widget>[
      SizedBox(width: 9999),
      wrap('Esc', () {
        inputModel.inputKey('VK_ESCAPE');
      }),
      wrap('Tab', () {
        inputModel.inputKey('VK_TAB');
      }),
      wrap('Home', () {
        inputModel.inputKey('VK_HOME');
      }),
      wrap('End', () {
        inputModel.inputKey('VK_END');
      }),
      wrap('Ins', () {
        inputModel.inputKey('VK_INSERT');
      }),
      wrap('Del', () {
        inputModel.inputKey('VK_DELETE');
      }),
      wrap('PgUp', () {
        inputModel.inputKey('VK_PRIOR');
      }),
      wrap('PgDn', () {
        inputModel.inputKey('VK_NEXT');
      }),
      // to-do: support PrtScr on Mac
      if (isWin || isLinux)
        wrap('PrtScr', () {
          inputModel.inputKey('VK_SNAPSHOT');
        }),
      if (isWin || isLinux)
        wrap('ScrollLock', () {
          inputModel.inputKey('VK_SCROLL');
        }),
      if (isWin || isLinux)
        wrap('Pause', () {
          inputModel.inputKey('VK_PAUSE');
        }),
      if (isWin || isLinux)
        // Maybe it's better to call it "Menu"
        // https://en.wikipedia.org/wiki/Menu_key
        wrap('Menu', () {
          inputModel.inputKey('Apps');
        }),
      wrap('Enter', () {
        inputModel.inputKey('VK_ENTER');
      }),
      SizedBox(width: 9999),
      wrap('', () {
        inputModel.inputKey('VK_LEFT');
      }, icon: Icons.keyboard_arrow_left),
      wrap('', () {
        inputModel.inputKey('VK_UP');
      }, icon: Icons.keyboard_arrow_up),
      wrap('', () {
        inputModel.inputKey('VK_DOWN');
      }, icon: Icons.keyboard_arrow_down),
      wrap('', () {
        inputModel.inputKey('VK_RIGHT');
      }, icon: Icons.keyboard_arrow_right),
      wrap(isMac ? 'Cmd+C' : 'Ctrl+C', () {
        sendPrompt(isMac, 'VK_C');
      }),
      wrap(isMac ? 'Cmd+V' : 'Ctrl+V', () {
        sendPrompt(isMac, 'VK_V');
      }),
      wrap(isMac ? 'Cmd+S' : 'Ctrl+S', () {
        sendPrompt(isMac, 'VK_S');
      }),
    ];
    final space = size.width > 320 ? 4.0 : 2.0;
    // 500 ms is long enough for this widget to be built!
    Future.delayed(Duration(milliseconds: 500), () {
      _updateRect();
    });
    return Container(
        key: _key,
        color: Color(0xAA000000),
        padding: EdgeInsets.only(
            top: _keyboardVisibilityController.isVisible ? 24 : 4, bottom: 8),
        child: Wrap(
          spacing: space,
          runSpacing: space,
          children: <Widget>[SizedBox(width: 9999)] +
              modifiers +
              keys +
              (_fn ? fn : []) +
              (_more ? more : []),
        ));
  }
}

class ImagePaint extends StatelessWidget {
  final FfiModel ffiModel;
  ImagePaint({Key? key, required this.ffiModel}) : super(key: key);

  @override
  Widget build(BuildContext context) {
    final m = Provider.of<ImageModel>(context);
    final c = Provider.of<CanvasModel>(context);
    var s = c.scale;
    final allRect = ffiModel.rect;
    if (ffiModel.pi.currentDisplay == kAllDisplayValue &&
        m.displayImages.isNotEmpty &&
        allRect != null) {
      // All displays in one view: each picture at its Windows position.
      final displays = ffiModel.pi.displays;
      final tiles = <DisplayTile>[];
      m.displayImages.forEach((i, image) {
        if (i < 0 || i >= displays.length) return;
        final d = displays[i];
        // A Linux peer's rect is in scaled units (FfiModel._getDisplaysRect).
        final k = ffiModel.isPeerLinux ? d.scale : 1.0;
        tiles.add(DisplayTile(
            image,
            Rect.fromLTWH(d.x - allRect.left, d.y - allRect.top, d.width / k,
                d.height / k)));
      });
      return CustomPaint(
        painter: MultiImagePainter(
            tiles: tiles,
            x: c.x / s,
            y: (c.y + c.getAdjustY()) / s,
            scale: s),
      );
    }
    if (ffiModel.isPeerLinux) {
      final displays = ffiModel.pi.getCurDisplays();
      if (displays.isNotEmpty) {
        s = s / displays[0].scale;
      }
    }
    final adjust = c.getAdjustY();
    // Android surface decoding: the display's picture is in a texture the hardware decoder
    // renders into. Same place and size as ImagePainter gives the software picture.
    final textureId = m.surfaceTextureId;
    if (textureId != null && ffiModel.pi.currentDisplay != kAllDisplayValue) {
      final left = (c.x / s).truncateToDouble() * s;
      final top = ((c.y + adjust) / s).truncateToDouble() * s;
      final size = m.surfaceSize * s;
      if (left.isFinite && top.isFinite && size.width > 0 && size.height > 0) {
        Widget texture = Texture(
            textureId: textureId,
            filterQuality: (s - 1.0).abs() > 0.001
                ? FilterQuality.medium
                : FilterQuality.none);
        if (m.surfaceBufX > 1 || m.surfaceBufY > 1) {
          // The texture shows the decoder's whole buffer, padding included: lay the buffer
          // out larger than the picture's place and cut the padding off.
          final bw = size.width * m.surfaceBufX;
          final bh = size.height * m.surfaceBufY;
          texture = ClipRect(
              child: OverflowBox(
                  alignment: Alignment.topLeft,
                  minWidth: bw,
                  maxWidth: bw,
                  minHeight: bh,
                  maxHeight: bh,
                  child: texture));
        }
        return Positioned(
            left: left,
            top: top,
            width: size.width,
            height: size.height,
            child: IgnorePointer(child: texture));
      }
    }
    return CustomPaint(
      painter: ImagePainter(
          image: m.image, x: c.x / s, y: (c.y + adjust) / s, scale: s),
    );
  }
}

class CursorPaint extends StatelessWidget {
  late final String id;
  CursorPaint(this.id);

  @override
  Widget build(BuildContext context) {
    final m = Provider.of<CursorModel>(context);
    final c = Provider.of<CanvasModel>(context);
    final ffiModel = Provider.of<FfiModel>(context);
    final s = c.scale;
    double hotx = m.hotx;
    double hoty = m.hoty;
    var image = m.image;
    if (image == null) {
      if (preDefaultCursor.image != null) {
        image = preDefaultCursor.image;
        hotx = preDefaultCursor.image!.width / 2;
        hoty = preDefaultCursor.image!.height / 2;
      }
    }
    if (preForbiddenCursor.image != null &&
        !ffiModel.viewOnly &&
        !ffiModel.keyboard &&
        !ShowRemoteCursorState.find(id).value) {
      image = preForbiddenCursor.image;
      hotx = preForbiddenCursor.image!.width / 2;
      hoty = preForbiddenCursor.image!.height / 2;
    }
    if (image == null) {
      return Offstage();
    }

    final minSize = 12.0;
    double mins =
        minSize / (image.width > image.height ? image.width : image.height);
    double factor = 1.0;
    if (s < mins) {
      factor = s / mins;
    }
    final s2 = s < mins ? mins : s;
    final adjust = c.getAdjustY();
    return CustomPaint(
      painter: ImagePainter(
          image: image,
          x: (m.x - hotx) * factor + c.x / s2,
          y: (m.y - hoty) * factor + (c.y + adjust) / s2,
          scale: s2),
    );
  }
}

void showOptions(
    BuildContext context, String id, OverlayDialogManager dialogManager) async {
  var displays = <Widget>[];
  final pi = gFFI.ffiModel.pi;
  final image = gFFI.ffiModel.getConnectionImageText();
  if (image != null) {
    displays.add(Padding(padding: const EdgeInsets.only(top: 8), child: image));
  }
  final privacyModeState = PrivacyModeState.find(id);
  if (pi.displays.length > 1 &&
      (privacyModeState.isEmpty ||
          allowDisplaySwitchInPrivacyMode(pi, privacyModeState.value))) {
    final cur = pi.currentDisplay;
    final children = <Widget>[];
    final isDarkTheme = MyTheme.currentThemeMode() == ThemeMode.dark;
    final numColorSelected = Colors.white;
    final numColorUnselected = isDarkTheme ? Colors.grey : Colors.black87;
    // We can't use `Theme.of(context).primaryColor` here, the color is:
    // - light theme: 0xff2196f3 (Colors.blue)
    // - dark theme: 0xff212121 (the canvas color?)
    final numBgSelected =
        Theme.of(context).colorScheme.primary.withOpacity(0.6);
    Widget displayButton(int i, String label, {double? width}) => InkWell(
        onTap: () {
          if (i == cur) return;
          openMonitorInTheSameTab(i, gFFI, pi);
          gFFI.dialogManager.dismissAll();
        },
        child: Ink(
            width: width,
            height: 40,
            padding: width == null
                ? const EdgeInsets.symmetric(horizontal: 10)
                : null,
            decoration: BoxDecoration(
                border: Border.all(color: Theme.of(context).hintColor),
                borderRadius: BorderRadius.circular(2),
                color: i == cur ? numBgSelected : null),
            child: Center(
                widthFactor: 1,
                child: Text(label,
                    style: TextStyle(
                        color:
                            i == cur ? numColorSelected : numColorUnselected,
                        fontWeight: FontWeight.bold)))));
    for (var i = 0; i < pi.displays.length; ++i) {
      children.add(displayButton(i, (i + 1).toString(), width: 40));
    }
    displays.add(Padding(
        padding: const EdgeInsets.only(top: 8),
        child: Wrap(
          alignment: WrapAlignment.center,
          spacing: 8,
          children: children,
        )));
  }
  if (displays.isNotEmpty) {
    displays.add(const Divider(color: MyTheme.border));
  }

  List<TRadioMenu<String>> viewStyleRadios =
      await toolbarViewStyle(context, id, gFFI);
  List<TRadioMenu<String>> imageQualityRadios =
      await toolbarImageQuality(context, id, gFFI);
  List<TRadioMenu<String>> codecRadios = await toolbarCodec(context, id, gFFI);
  List<TToggleMenu> cursorToggles = await toolbarCursor(context, id, gFFI);
  List<TToggleMenu> displayToggles =
      await toolbarDisplayToggle(context, id, gFFI);
  if (isMobile) {
    displayToggles.insert(
        0,
        TToggleMenu(
            child: Text(translate('Lock canvas')),
            value: gFFI.canvasModel.locked,
            onChanged: (value) => gFFI.canvasModel.setLocked(value == true)));
  }

  List<TToggleMenu> privacyModeList = [];
  if ((gFFI.ffiModel.pi.features.privacyMode && gFFI.ffiModel.keyboard) ||
      privacyModeState.isNotEmpty) {
    privacyModeList = toolbarPrivacyMode(privacyModeState, context, id, gFFI);
    if (privacyModeList.length == 1) {
      displayToggles.add(privacyModeList[0]);
    }
  }

  dialogManager.show((setState, close, context) {
    var viewStyle =
        (viewStyleRadios.isNotEmpty ? viewStyleRadios[0].groupValue : '').obs;
    var imageQuality =
        (imageQualityRadios.isNotEmpty ? imageQualityRadios[0].groupValue : '')
            .obs;
    var codec = (codecRadios.isNotEmpty ? codecRadios[0].groupValue : '').obs;
    final radios = [
      for (var e in viewStyleRadios)
        Obx(() => getRadio<String>(
            e.child,
            e.value,
            viewStyle.value,
            e.onChanged != null
                ? (v) {
                    e.onChanged?.call(v);
                    if (v != null) viewStyle.value = v;
                  }
                : null)),
      // Show custom scale controls when custom view style is selected
      Obx(() => viewStyle.value == kRemoteViewStyleCustom
          ? MobileCustomScaleControls(ffi: gFFI)
          : const SizedBox.shrink()),
      const Divider(color: MyTheme.border),
      for (var e in imageQualityRadios)
        Obx(() => getRadio<String>(
            e.child,
            e.value,
            imageQuality.value,
            e.onChanged != null
                ? (v) {
                    e.onChanged?.call(v);
                    if (v != null) imageQuality.value = v;
                  }
                : null)),
      const Divider(color: MyTheme.border),
      for (var e in codecRadios)
        Obx(() => getRadio<String>(
            e.child,
            e.value,
            codec.value,
            e.onChanged != null
                ? (v) {
                    e.onChanged?.call(v);
                    if (v != null) codec.value = v;
                  }
                : null)),
      if (codecRadios.isNotEmpty) const Divider(color: MyTheme.border),
    ];
    final rxCursorToggleValues = cursorToggles.map((e) => e.value.obs).toList();
    final cursorTogglesList = cursorToggles
        .asMap()
        .entries
        .map((e) => Obx(() => CheckboxListTile(
            contentPadding: EdgeInsets.zero,
            visualDensity: VisualDensity.compact,
            value: rxCursorToggleValues[e.key].value,
            onChanged: e.value.onChanged != null
                ? (v) {
                    e.value.onChanged?.call(v);
                    if (v != null) rxCursorToggleValues[e.key].value = v;
                  }
                : null,
            title: e.value.child)))
        .toList();

    final rxToggleValues = displayToggles.map((e) => e.value.obs).toList();
    final displayTogglesList = displayToggles
        .asMap()
        .entries
        .map((e) => Obx(() => CheckboxListTile(
            contentPadding: EdgeInsets.zero,
            visualDensity: VisualDensity.compact,
            value: rxToggleValues[e.key].value,
            onChanged: e.value.onChanged != null
                ? (v) {
                    e.value.onChanged?.call(v);
                    if (v != null) rxToggleValues[e.key].value = v;
                  }
                : null,
            title: e.value.child)))
        .toList();
    final toggles = [
      ...cursorTogglesList,
      if (cursorToggles.isNotEmpty) const Divider(color: MyTheme.border),
      ...displayTogglesList,
    ];

    Widget privacyModeWidget = Offstage();
    if (privacyModeList.length > 1) {
      privacyModeWidget = ListTile(
        contentPadding: EdgeInsets.zero,
        visualDensity: VisualDensity.compact,
        title: Text(translate('Privacy mode')),
        onTap: () => setPrivacyModeDialog(
            dialogManager, privacyModeList, privacyModeState),
      );
    }

    var popupDialogMenus = List<Widget>.empty(growable: true);
    final resolution = getResolutionMenu(gFFI, id);
    if (resolution != null) {
      popupDialogMenus.add(ListTile(
        contentPadding: EdgeInsets.zero,
        visualDensity: VisualDensity.compact,
        title: resolution.child,
        onTap: () {
          close();
          resolution.onPressed?.call();
        },
      ));
    }
    final virtualDisplayMenu = getVirtualDisplayMenu(gFFI, id);
    if (virtualDisplayMenu != null) {
      popupDialogMenus.add(ListTile(
        contentPadding: EdgeInsets.zero,
        visualDensity: VisualDensity.compact,
        title: virtualDisplayMenu.child,
        onTap: () {
          close();
          virtualDisplayMenu.onPressed?.call();
        },
      ));
    }
    if (popupDialogMenus.isNotEmpty) {
      popupDialogMenus.add(const Divider(color: MyTheme.border));
    }

    return CustomAlertDialog(
      content: Column(
          mainAxisSize: MainAxisSize.min,
          children: displays +
              radios +
              popupDialogMenus +
              toggles +
              [privacyModeWidget]),
    );
  }, clickMaskDismiss: true, backDismiss: true).then((value) {
    _disableAndroidSoftKeyboard();
  });
}

TTextMenu? getVirtualDisplayMenu(FFI ffi, String id) {
  if (!showVirtualDisplayMenu(ffi)) {
    return null;
  }
  return TTextMenu(
    child: Text(translate("Virtual display")),
    onPressed: () {
      ffi.dialogManager.show((setState, close, context) {
        final children = getVirtualDisplayMenuChildren(ffi, id, close);
        return CustomAlertDialog(
          title: Text(translate('Virtual display')),
          content: Column(
            mainAxisSize: MainAxisSize.min,
            children: children,
          ),
        );
      }, clickMaskDismiss: true, backDismiss: true).then((value) {
        _disableAndroidSoftKeyboard();
      });
    },
  );
}

TTextMenu? getResolutionMenu(FFI ffi, String id) {
  final ffiModel = ffi.ffiModel;
  final pi = ffiModel.pi;
  final resolutions = pi.resolutions;
  final display = pi.tryGetDisplayIfNotAllDisplay(display: pi.currentDisplay);

  final visible =
      ffiModel.keyboard && (resolutions.length > 1) && display != null;
  if (!visible) return null;

  return TTextMenu(
    child: Text(translate("Resolution")),
    onPressed: () {
      ffi.dialogManager.show((setState, close, context) {
        final children = resolutions
            .map((e) => getRadio<String>(
                  Text('${e.width}x${e.height}'),
                  '${e.width}x${e.height}',
                  '${display.width}x${display.height}',
                  (value) {
                    close();
                    bind.sessionChangeResolution(
                      sessionId: ffi.sessionId,
                      display: pi.currentDisplay,
                      width: e.width,
                      height: e.height,
                    );
                  },
                ))
            .toList();
        return CustomAlertDialog(
          title: Text(translate('Resolution')),
          content: Column(
            mainAxisSize: MainAxisSize.min,
            children: children,
          ),
        );
      }, clickMaskDismiss: true, backDismiss: true).then((value) {
        _disableAndroidSoftKeyboard();
      });
    },
  );
}

void sendPrompt(bool isMac, String key) {
  // Keeps a locked modifier, uses up a one-shot one (see InputModel.cycleModifier).
  gFFI.inputModel.inputShortcut(key, isMac: isMac);
}

/// One key of the Quest toolbar keys (see _RemotePageState._keySpecs).
class _KeySpec {
  const _KeySpec(this.label, this.onTap, {this.icon, this.state = 0});

  final String label;
  final VoidCallback onTap;
  final IconData? icon;

  /// 0 = plain key, 1 = modifier on for the next key, 2 = modifier locked / toggle on.
  final int state;
}

/// Reports the laid-out height of [child] after each layout that changes it.
class _HeightReporter extends SingleChildRenderObjectWidget {
  const _HeightReporter({required this.onHeight, super.child});

  final ValueChanged<double> onHeight;

  @override
  RenderObject createRenderObject(BuildContext context) =>
      _RenderHeightReporter(onHeight);

  @override
  void updateRenderObject(
      BuildContext context, covariant _RenderHeightReporter renderObject) {
    renderObject.onHeight = onHeight;
  }
}

class _RenderHeightReporter extends RenderProxyBox {
  _RenderHeightReporter(this.onHeight);

  ValueChanged<double> onHeight;
  double? _reported;

  @override
  void performLayout() {
    super.performLayout();
    final h = size.height;
    if (_reported == h) return;
    _reported = h;
    // Not during layout: the callback rebuilds what depends on the canvas size.
    WidgetsBinding.instance.addPostFrameCallback((_) {
      if (attached && _reported == h) onHeight(h);
    });
  }
}

class FABLocation extends FloatingActionButtonLocation {
  FloatingActionButtonLocation location;
  double offsetX;
  double offsetY;
  FABLocation(this.location, this.offsetX, this.offsetY);

  @override
  Offset getOffset(ScaffoldPrelayoutGeometry scaffoldGeometry) {
    final offset = location.getOffset(scaffoldGeometry);
    return Offset(offset.dx + offsetX, offset.dy + offsetY);
  }
}


/// Headset battery and Wi-Fi in the toolbar (Quest main window), read every 30 s from
/// MainActivity's `quest_status`. A tap shows the details.
class _QuestStatusChip extends StatefulWidget {
  const _QuestStatusChip();

  @override
  State<_QuestStatusChip> createState() => _QuestStatusChipState();
}

class _QuestStatusChipState extends State<_QuestStatusChip> {
  Timer? _timer;
  Map<dynamic, dynamic> _s = const {};

  @override
  void initState() {
    super.initState();
    _read();
    _timer = Timer.periodic(const Duration(seconds: 30), (_) => _read());
  }

  @override
  void dispose() {
    _timer?.cancel();
    super.dispose();
  }

  Future<void> _read() async {
    try {
      final r = await gFFI.invokeMethodWithResult<dynamic>('quest_status');
      if (mounted && r is Map) setState(() => _s = r);
    } catch (e) {
      debugPrint('[quest] status: $e');
    }
  }

  IconData _batteryIcon(int level, bool charging) {
    if (charging) return Icons.battery_charging_full;
    if (level >= 95) return Icons.battery_full;
    if (level >= 80) return Icons.battery_6_bar;
    if (level >= 65) return Icons.battery_5_bar;
    if (level >= 50) return Icons.battery_4_bar;
    if (level >= 35) return Icons.battery_3_bar;
    if (level >= 20) return Icons.battery_2_bar;
    if (level >= 10) return Icons.battery_1_bar;
    return Icons.battery_alert;
  }

  IconData _wifiIcon(bool wifi, int? rssi) {
    if (!wifi) return Icons.wifi_off;
    if (rssi == null || rssi >= -55) return Icons.wifi;
    if (rssi >= -66) return Icons.network_wifi_3_bar;
    if (rssi >= -77) return Icons.network_wifi_2_bar;
    return Icons.network_wifi_1_bar;
  }

  @override
  Widget build(BuildContext context) {
    final level = _s['battery'] as int?;
    if (level == null) return const SizedBox.shrink();
    final charging = _s['charging'] == true;
    final wifi = _s['wifi'] == true;
    final rssi = _s['rssi'] as int?;
    final low = level <= 20 && !charging;
    final color = low ? Colors.redAccent : Colors.white;
    String details() {
      final b = 'Akkumulátor: $level%${charging ? ' (töltődik)' : ''}';
      if (!wifi) return '$b\nWi-Fi: nincs';
      final mbps = _s['linkMbps'];
      final mhz = _s['freqMHz'] as int?;
      final band = mhz == null
          ? ''
          : mhz >= 5925
              ? ', 6 GHz'
              : mhz >= 4900
                  ? ', 5 GHz'
                  : ', 2,4 GHz';
      return '$b\nWi-Fi: ${rssi ?? '?'} dBm, ${mbps ?? '?'} Mbps$band';
    }

    return InkWell(
      onTap: () {
        _read();
        showToast(details());
      },
      child: Padding(
        padding: const EdgeInsets.symmetric(horizontal: 6),
        child: Row(mainAxisSize: MainAxisSize.min, children: [
          Icon(_batteryIcon(level, charging), color: color, size: 20),
          Text('$level%', style: TextStyle(color: color, fontSize: 13)),
          const SizedBox(width: 6),
          Icon(_wifiIcon(wifi, rssi), color: Colors.white, size: 18),
        ]),
      ),
    );
  }
}
