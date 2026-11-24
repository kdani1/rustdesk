import 'dart:async';
import 'dart:convert';
import 'dart:math';

import 'package:back_button_interceptor/back_button_interceptor.dart';
import 'package:desktop_multi_window/desktop_multi_window.dart';
import 'package:flutter/foundation.dart';
import 'package:flutter/gestures.dart';
import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_hbb/common/formatter/id_formatter.dart';
import 'package:flutter_hbb/desktop/widgets/refresh_wrapper.dart';
import 'package:flutter_hbb/desktop/widgets/tabbar_widget.dart';
import 'package:flutter_hbb/main.dart';
import 'package:flutter_hbb/models/peer_model.dart';
import 'package:flutter_hbb/models/peer_tab_model.dart';
import 'package:flutter_hbb/models/state_model.dart';
import 'package:flutter_hbb/utils/multi_window_manager.dart';
import 'package:flutter_hbb/utils/platform_channel.dart';
import 'package:flutter_svg/flutter_svg.dart';
import 'package:get/get.dart';
import 'package:get/get_rx/src/rx_workers/utils/debouncer.dart';
import 'package:provider/provider.dart';
import 'package:uni_links/uni_links.dart';
import 'package:url_launcher/url_launcher.dart';
import 'package:uuid/uuid.dart';
import 'package:wakelock_plus/wakelock_plus.dart';
import 'package:window_manager/window_manager.dart';
import 'package:window_size/window_size.dart' as window_size;

import '../consts.dart';
import 'common/widgets/overlay.dart';
import 'common/widgets/toolbar.dart'
    show toggleAmyuniVirtualDisplay, showVirtualDisplayMenu;
import 'mobile/pages/file_manager_page.dart';
import 'mobile/pages/remote_page.dart';
import 'mobile/pages/view_camera_page.dart';
import 'mobile/pages/terminal_page.dart';
import 'desktop/pages/remote_page.dart' as desktop_remote;
import 'desktop/pages/file_manager_page.dart' as desktop_file_manager;
import 'desktop/pages/view_camera_page.dart' as desktop_view_camera;
import 'package:flutter_hbb/desktop/widgets/remote_toolbar.dart';
import 'models/model.dart';
import 'models/platform_model.dart';

import 'package:flutter_hbb/native/win32.dart'
    if (dart.library.html) 'package:flutter_hbb/web/win32.dart';
import 'package:flutter_hbb/native/common.dart'
    if (dart.library.html) 'package:flutter_hbb/web/common.dart';
import 'package:flutter_hbb/utils/http_service.dart' as http;

final globalKey = GlobalKey<NavigatorState>();
final navigationBarKey = GlobalKey();

final isAndroid = isAndroid_;
final isIOS = isIOS_;
final isWindows = isWindows_;
final isMacOS = isMacOS_;
final isLinux = isLinux_;
final isDesktop = isDesktop_;
final isWeb = isWeb_;
final isWebDesktop = isWebDesktop_;
final isWebOnWindows = isWebOnWindows_;
final isWebOnLinux = isWebOnLinux_;
final isWebOnMacOs = isWebOnMacOS_;
var isMobile = isAndroid || isIOS;
var version = '';
// Fork: the build number of this app (pubspec version after '+'), shown in About.
var buildNumber = '';
int androidVersion = 0;

/// Fork: [v] with this app's build number appended, as "1.5.0 (build 107)".
String withBuildNumber(String v) =>
    buildNumber.isEmpty ? v : '$v (build $buildNumber)';

// Only used on Linux.
// `windowManager.setResizable(false)` will reset the window size to the default size on Linux.
// https://stackoverflow.com/questions/8193613/gtk-window-resize-disable-without-going-back-to-default
// So we need to use this flag to enable/disable resizable.
bool _linuxWindowResizable = true;

// Only used on Windows(window manager).
bool _ignoreDevicePixelRatio = true;

/// only available for Windows target
int windowsBuildNumber = 0;
DesktopType? desktopType;

// Tolerance used for floating-point position comparisons to avoid precision errors.
const double _kPositionEpsilon = 1e-6;

bool get isMainDesktopWindow =>
    desktopType == DesktopType.main || desktopType == DesktopType.cm;

/// Check if the app is running with single view mode.
bool isSingleViewApp() {
  return desktopType == DesktopType.cm;
}

/// * debug or test only, DO NOT enable in release build
bool isTest = false;

typedef F = String Function(String);
typedef FMethod = String Function(String, dynamic);

typedef StreamEventHandler = Future<void> Function(Map<String, dynamic>);
typedef SessionID = UuidValue;
final iconHardDrive = MemoryImage(Uint8List.fromList(base64Decode(
    'iVBORw0KGgoAAAANSUhEUgAAAMgAAADICAMAAACahl6sAAAAmVBMVEUAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAjHWqVAAAAMnRSTlMAv0BmzLJNXlhiUu2fxXDgu7WuSUUe29LJvpqUjX53VTstD7ilNujCqTEk5IYH+vEoFjKvAagAAAPpSURBVHja7d0JbhpBEIXhB3jYzb5vBgzYgO04df/DJXGUKMwU9ECmZ6pQfSfw028LCXW3YYwxxhhjjDHGGGOM0eZ9VV1MckdKWLM1bRQ/35GW/WxHHu1me6ShuyHvNl34VhlTKsYVeDWj1EzgUZ1S1DrAk/UDparZgxd9Sl0BHnxSBhpI3jfKQG2FpLUpE69I2ILikv1nsvygjBwPSNKYMlNHggqUoSKS80AZCnwHqQ1zCRvW+CRegwRFeFAMKKrtM8gTPJlzSfwFgT9dJom3IDN4VGaSeAryAK8m0SSeghTg1ZYiql6CjBDhO8mzlyAVhKhIwgXxrh5NojGIhyRckEdwpCdhgpSQgiWTRGMQNonGIGySp0SDvMDBX5KWxiB8Eo1BgE00SYJBykhNnkmSWJAcLpGaJNMgfJKyxiDAK4WNEwryhMtkJsk8CJtEYxA+icYgQIfCcgkEqcJNXhIRQdgkGoPwSTQG+e8khdu/7JOVREwQIKCwF41B2CQljUH4JLcH6SI+OUlEBQHa0SQag/BJNAbhkjxqDMIn0RgEeI4muSlID9eSkERgEKAVTaIxCJ9EYxA2ydVB8hCASVLRGAQYR5NoDMIn0RgEyFHYSGMQPonGII4kziCNvBgNJonEk4u3GAk8Sprk6eYaqbMDY0oKvUm5jfC/viGiSypV7+M3i2iDsAGpNEDYjlTa3W8RdR/r544g50ilnA0RxoZIE2NIXqQbhkAkGyKNDZHGhkhjQ6SxIdLYEGlsiDQ2JGTVeD0264U9zipPh7XOooffpA6pfNCXjxl4/c3pUzlChwzor53zwYYVfpI5pOV6LWFF/2jiJ5FDSs5jdY/0rwUAkUMeXWdBqnSqD0DikBqdqCHsjTvELm9In0IOri/0pwAEDtlSyNaRjAIAAoesKWTtuusxByBwCJp0oomwBXcYUuCQgE50ENajE4OvZAKHLB1/68Br5NqiyCGYOY8YRd77kTkEb64n7lZN+mOIX4QOwb5FX0ZVx3uOxwW+SB0CbBubemWP8/rlaaeRX+M3uUOuZENsiA25zIbYkPsZElBIHwL13U/PTjJ/cyOOEoVM3I+hziDQlELm7pPxw3eI8/7gPh1fpLA6xGnEeDDgO0UcIAzzM35HxLPIq5SXe9BLzOsj9eUaQqyXzxS1QFSfWM2cCANiHcAISJ0AnCKpUwTuIkkA3EeSInAXSQKcs1V18e24wlllUmQp9v9zXKeHi+akRAMOPVKhAqdPBZeUmnnEsO6QcJ0+4qmOSbBxFfGVRiTUqITrdKcCbyYO3/K4wX4+aQ+FfNjXhu3JfAVjjDHGGGOMMcYYY4xIPwCgfqT6TbhCLAAAAABJRU5ErkJggg==')));

enum DesktopType {
  main,
  remote,
  fileTransfer,
  viewCamera,
  terminal,
  cm,
  portForward,
}

bool isDoubleEqual(double a, double b) {
  return (a - b).abs() < _kPositionEpsilon;
}

class IconFont {
  static const _family1 = 'Tabbar';
  static const _family2 = 'PeerSearchbar';
  static const _family3 = 'AddressBook';
  static const _family4 = 'DeviceGroup';
  static const _family5 = 'More';

  IconFont._();

  static const IconData max = IconData(0xe606, fontFamily: _family1);
  static const IconData restore = IconData(0xe607, fontFamily: _family1);
  static const IconData close = IconData(0xe668, fontFamily: _family1);
  static const IconData min = IconData(0xe609, fontFamily: _family1);
  static const IconData add = IconData(0xe664, fontFamily: _family1);
  static const IconData menu = IconData(0xe628, fontFamily: _family1);
  static const IconData search = IconData(0xe6a4, fontFamily: _family2);
  static const IconData roundClose = IconData(0xe6ed, fontFamily: _family2);
  static const IconData addressBook = IconData(0xe602, fontFamily: _family3);
  static const IconData deviceGroupOutline =
      IconData(0xe623, fontFamily: _family4);
  static const IconData deviceGroupFill =
      IconData(0xe748, fontFamily: _family4);
  static const IconData more = IconData(0xe609, fontFamily: _family5);
}

class ColorThemeExtension extends ThemeExtension<ColorThemeExtension> {
  const ColorThemeExtension({
    required this.border,
    required this.border2,
    required this.border3,
    required this.highlight,
    required this.drag_indicator,
    required this.shadow,
    required this.errorBannerBg,
    required this.me,
    required this.toastBg,
    required this.toastText,
    required this.divider,
  });

  final Color? border;
  final Color? border2;
  final Color? border3;
  final Color? highlight;
  final Color? drag_indicator;
  final Color? shadow;
  final Color? errorBannerBg;
  final Color? me;
  final Color? toastBg;
  final Color? toastText;
  final Color? divider;

  static final light = ColorThemeExtension(
    border: Color(0xFFCCCCCC),
    border2: Color(0xFFBBBBBB),
    border3: Colors.black26,
    highlight: Color(0xFFE5E5E5),
    drag_indicator: Colors.grey[800],
    shadow: Colors.black,
    errorBannerBg: Color(0xFFFDEEEB),
    me: Colors.green,
    toastBg: Colors.black.withOpacity(0.6),
    toastText: Colors.white,
    divider: Colors.black38,
  );

  static final dark = ColorThemeExtension(
    border: Color(0xFF555555),
    border2: Color(0xFFE5E5E5),
    border3: Colors.white24,
    highlight: Color(0xFF3F3F3F),
    drag_indicator: Colors.grey,
    shadow: Colors.grey,
    errorBannerBg: Color(0xFF470F2D),
    me: Colors.greenAccent,
    toastBg: Colors.white.withOpacity(0.6),
    toastText: Colors.black,
    divider: Colors.white38,
  );

  @override
  ThemeExtension<ColorThemeExtension> copyWith({
    Color? border,
    Color? border2,
    Color? border3,
    Color? highlight,
    Color? drag_indicator,
    Color? shadow,
    Color? errorBannerBg,
    Color? me,
    Color? toastBg,
    Color? toastText,
    Color? divider,
  }) {
    return ColorThemeExtension(
      border: border ?? this.border,
      border2: border2 ?? this.border2,
      border3: border3 ?? this.border3,
      highlight: highlight ?? this.highlight,
      drag_indicator: drag_indicator ?? this.drag_indicator,
      shadow: shadow ?? this.shadow,
      errorBannerBg: errorBannerBg ?? this.errorBannerBg,
      me: me ?? this.me,
      toastBg: toastBg ?? this.toastBg,
      toastText: toastText ?? this.toastText,
      divider: divider ?? this.divider,
    );
  }

  @override
  ThemeExtension<ColorThemeExtension> lerp(
      ThemeExtension<ColorThemeExtension>? other, double t) {
    if (other is! ColorThemeExtension) {
      return this;
    }
    return ColorThemeExtension(
      border: Color.lerp(border, other.border, t),
      border2: Color.lerp(border2, other.border2, t),
      border3: Color.lerp(border3, other.border3, t),
      highlight: Color.lerp(highlight, other.highlight, t),
      drag_indicator: Color.lerp(drag_indicator, other.drag_indicator, t),
      shadow: Color.lerp(shadow, other.shadow, t),
      errorBannerBg: Color.lerp(shadow, other.errorBannerBg, t),
      me: Color.lerp(shadow, other.me, t),
      toastBg: Color.lerp(shadow, other.toastBg, t),
      toastText: Color.lerp(shadow, other.toastText, t),
      divider: Color.lerp(shadow, other.divider, t),
    );
  }
}

class MyTheme {
  MyTheme._();

  static const Color grayBg = Color(0xFFEFEFF2);
  static const Color accent = Color(0xFF0071FF);
  static const Color accent50 = Color(0x770071FF);
  static const Color accent80 = Color(0xAA0071FF);
  static const Color canvasColor = Color(0xFF212121);
  static const Color border = Color(0xFFCCCCCC);
  static const Color idColor = Color(0xFF00B6F0);
  static const Color darkGray = Color.fromARGB(255, 148, 148, 148);
  static const Color cmIdColor = Color(0xFF21790B);
  static const Color dark = Colors.black87;
  static const Color button = Color(0xFF2C8CFF);
  static const Color hoverBorder = Color(0xFF999999);

  // ListTile
  static const ListTileThemeData listTileTheme = ListTileThemeData(
    shape: RoundedRectangleBorder(
      borderRadius: BorderRadius.all(
        Radius.circular(5),
      ),
    ),
  );

  static SwitchThemeData switchTheme() {
    return SwitchThemeData(
        splashRadius: (isDesktop || isWebDesktop) ? 0 : kRadialReactionRadius);
  }

  static RadioThemeData radioTheme() {
    return RadioThemeData(
        splashRadius: (isDesktop || isWebDesktop) ? 0 : kRadialReactionRadius);
  }

  // Checkbox
  static const CheckboxThemeData checkboxTheme = CheckboxThemeData(
    splashRadius: 0,
    shape: RoundedRectangleBorder(
      borderRadius: BorderRadius.all(
        Radius.circular(5),
      ),
    ),
  );

  // TextButton
  // Value is used to calculate "dialog.actionsPadding"
  static const double mobileTextButtonPaddingLR = 20;

  // TextButton on mobile needs a fixed padding, otherwise small buttons
  // like "OK" has a larger left/right padding.
  static TextButtonThemeData mobileTextButtonTheme = TextButtonThemeData(
    style: TextButton.styleFrom(
      padding: EdgeInsets.symmetric(horizontal: mobileTextButtonPaddingLR),
      shape: RoundedRectangleBorder(
        borderRadius: BorderRadius.circular(8.0),
      ),
    ),
  );

  //tooltip
  static TooltipThemeData tooltipTheme() {
    return TooltipThemeData(
      waitDuration: Duration(seconds: 1, milliseconds: 500),
    );
  }

  // Dialogs
  static const double dialogPadding = 24;

  // padding bottom depends on content (some dialogs has no content)
  static EdgeInsets dialogTitlePadding({bool content = true}) {
    final double p = dialogPadding;

    return EdgeInsets.fromLTRB(p, p, p, content ? 0 : p);
  }

  // padding bottom depends on actions (mobile has dialogs without actions)
  static EdgeInsets dialogContentPadding({bool actions = true}) {
    final double p = dialogPadding;

    return (isDesktop || isWebDesktop)
        ? EdgeInsets.fromLTRB(p, p, p, actions ? (p - 4) : p)
        : EdgeInsets.fromLTRB(p, p, p, actions ? (p / 2) : p);
  }

  static EdgeInsets dialogActionsPadding() {
    final double p = dialogPadding;

    return (isDesktop || isWebDesktop)
        ? EdgeInsets.fromLTRB(p, 0, p, (p - 4))
        : EdgeInsets.fromLTRB(p, 0, (p - mobileTextButtonPaddingLR), (p / 2));
  }

  static EdgeInsets dialogButtonPadding = (isDesktop || isWebDesktop)
      ? EdgeInsets.only(left: dialogPadding)
      : EdgeInsets.only(left: dialogPadding / 3);

  static ScrollbarThemeData scrollbarTheme = ScrollbarThemeData(
    thickness: MaterialStateProperty.all(6),
    thumbColor: MaterialStateProperty.resolveWith<Color?>((states) {
      if (states.contains(MaterialState.dragged)) {
        return Colors.grey[900];
      } else if (states.contains(MaterialState.hovered)) {
        return Colors.grey[700];
      } else {
        return Colors.grey[500];
      }
    }),
    crossAxisMargin: 4,
  );

  static ScrollbarThemeData scrollbarThemeDark = scrollbarTheme.copyWith(
    thumbColor: MaterialStateProperty.resolveWith<Color?>((states) {
      if (states.contains(MaterialState.dragged)) {
        return Colors.grey[100];
      } else if (states.contains(MaterialState.hovered)) {
        return Colors.grey[300];
      } else {
        return Colors.grey[500];
      }
    }),
  );

  static ThemeData lightTheme = ThemeData(
    // https://stackoverflow.com/questions/77537315/after-upgrading-to-flutter-3-16-the-app-bar-background-color-button-size-and
    useMaterial3: false,
    brightness: Brightness.light,
    hoverColor: Color.fromARGB(255, 224, 224, 224),
    scaffoldBackgroundColor: Colors.white,
    dialogBackgroundColor: Colors.white,
    appBarTheme: AppBarTheme(
      shadowColor: Colors.transparent,
    ),
    dialogTheme: DialogTheme(
      elevation: 15,
      shape: RoundedRectangleBorder(
        borderRadius: BorderRadius.circular(18.0),
        side: BorderSide(
          width: 1,
          color: grayBg,
        ),
      ),
    ),
    scrollbarTheme: scrollbarTheme,
    inputDecorationTheme: isDesktop
        ? InputDecorationTheme(
            fillColor: grayBg,
            filled: true,
            isDense: true,
            border: OutlineInputBorder(
              borderRadius: BorderRadius.circular(8),
            ),
          )
        : null,
    textTheme: const TextTheme(
        titleLarge: TextStyle(fontSize: 19, color: Colors.black87),
        titleSmall: TextStyle(fontSize: 14, color: Colors.black87),
        bodySmall: TextStyle(fontSize: 12, color: Colors.black87, height: 1.25),
        bodyMedium:
            TextStyle(fontSize: 14, color: Colors.black87, height: 1.25),
        labelLarge: TextStyle(fontSize: 16.0, color: MyTheme.accent80)),
    cardColor: grayBg,
    hintColor: Color(0xFFAAAAAA),
    visualDensity: VisualDensity.adaptivePlatformDensity,
    tabBarTheme: const TabBarTheme(
      labelColor: Colors.black87,
    ),
    tooltipTheme: tooltipTheme(),
    splashColor: (isDesktop || isWebDesktop) ? Colors.transparent : null,
    highlightColor: (isDesktop || isWebDesktop) ? Colors.transparent : null,
    splashFactory: (isDesktop || isWebDesktop) ? NoSplash.splashFactory : null,
    textButtonTheme: (isDesktop || isWebDesktop)
        ? TextButtonThemeData(
            style: TextButton.styleFrom(
              splashFactory: NoSplash.splashFactory,
              shape: RoundedRectangleBorder(
                borderRadius: BorderRadius.circular(18.0),
              ),
            ),
          )
        : mobileTextButtonTheme,
    elevatedButtonTheme: ElevatedButtonThemeData(
      style: ElevatedButton.styleFrom(
        backgroundColor: MyTheme.accent,
        shape: RoundedRectangleBorder(
          borderRadius: BorderRadius.circular(8.0),
        ),
      ),
    ),
    outlinedButtonTheme: OutlinedButtonThemeData(
      style: OutlinedButton.styleFrom(
        backgroundColor: grayBg,
        foregroundColor: Colors.black87,
        shape: RoundedRectangleBorder(
          borderRadius: BorderRadius.circular(8.0),
        ),
      ),
    ),
    switchTheme: switchTheme(),
    radioTheme: radioTheme(),
    checkboxTheme: checkboxTheme,
    listTileTheme: listTileTheme,
    menuBarTheme: MenuBarThemeData(
        style:
            MenuStyle(backgroundColor: MaterialStatePropertyAll(Colors.white))),
    colorScheme: ColorScheme.light(
        primary: Colors.blue, secondary: accent, background: grayBg),
    popupMenuTheme: PopupMenuThemeData(
        color: Colors.white,
        shape: RoundedRectangleBorder(
          side: BorderSide(
              color: (isDesktop || isWebDesktop)
                  ? Color(0xFFECECEC)
                  : Colors.transparent),
          borderRadius: BorderRadius.all(Radius.circular(8.0)),
        )),
  ).copyWith(
    extensions: <ThemeExtension<dynamic>>[
      ColorThemeExtension.light,
      TabbarTheme.light,
    ],
  );
  static ThemeData darkTheme = ThemeData(
    useMaterial3: false,
    brightness: Brightness.dark,
    hoverColor: Color.fromARGB(255, 45, 46, 53),
    scaffoldBackgroundColor: Color(0xFF18191E),
    dialogBackgroundColor: Color(0xFF18191E),
    appBarTheme: AppBarTheme(
      shadowColor: Colors.transparent,
    ),
    dialogTheme: DialogTheme(
      elevation: 15,
      shape: RoundedRectangleBorder(
        borderRadius: BorderRadius.circular(18.0),
        side: BorderSide(
          width: 1,
          color: Color(0xFF24252B),
        ),
      ),
    ),
    scrollbarTheme: scrollbarThemeDark,
    inputDecorationTheme: (isDesktop || isWebDesktop)
        ? InputDecorationTheme(
            fillColor: Color(0xFF24252B),
            filled: true,
            isDense: true,
            border: OutlineInputBorder(
              borderRadius: BorderRadius.circular(8),
            ),
          )
        : null,
    textTheme: const TextTheme(
      titleLarge: TextStyle(fontSize: 19),
      titleSmall: TextStyle(fontSize: 14),
      bodySmall: TextStyle(fontSize: 12, height: 1.25),
      bodyMedium: TextStyle(fontSize: 14, height: 1.25),
      labelLarge: TextStyle(
        fontSize: 16.0,
        fontWeight: FontWeight.bold,
        color: accent80,
      ),
    ),
    cardColor: Color(0xFF24252B),
    visualDensity: VisualDensity.adaptivePlatformDensity,
    tabBarTheme: const TabBarTheme(
      labelColor: Colors.white70,
    ),
    tooltipTheme: tooltipTheme(),
    splashColor: (isDesktop || isWebDesktop) ? Colors.transparent : null,
    highlightColor: (isDesktop || isWebDesktop) ? Colors.transparent : null,
    splashFactory: (isDesktop || isWebDesktop) ? NoSplash.splashFactory : null,
    textButtonTheme: (isDesktop || isWebDesktop)
        ? TextButtonThemeData(
            style: TextButton.styleFrom(
              splashFactory: NoSplash.splashFactory,
              disabledForegroundColor: Colors.white70,
              foregroundColor: Colors.white70,
              shape: RoundedRectangleBorder(
                borderRadius: BorderRadius.circular(18.0),
              ),
            ),
          )
        : mobileTextButtonTheme,
    elevatedButtonTheme: ElevatedButtonThemeData(
      style: ElevatedButton.styleFrom(
        backgroundColor: MyTheme.accent,
        foregroundColor: Colors.white,
        disabledForegroundColor: Colors.white70,
        disabledBackgroundColor: Colors.white10,
        shape: RoundedRectangleBorder(
          borderRadius: BorderRadius.circular(8.0),
        ),
      ),
    ),
    outlinedButtonTheme: OutlinedButtonThemeData(
      style: OutlinedButton.styleFrom(
        backgroundColor: Color(0xFF24252B),
        side: BorderSide(color: Colors.white12, width: 0.5),
        disabledForegroundColor: Colors.white70,
        foregroundColor: Colors.white70,
        shape: RoundedRectangleBorder(
          borderRadius: BorderRadius.circular(8.0),
        ),
      ),
    ),
    switchTheme: switchTheme(),
    radioTheme: radioTheme(),
    checkboxTheme: checkboxTheme,
    listTileTheme: listTileTheme,
    menuBarTheme: MenuBarThemeData(
        style: MenuStyle(
            backgroundColor: MaterialStatePropertyAll(Color(0xFF121212)))),
    colorScheme: ColorScheme.dark(
      primary: Colors.blue,
      secondary: accent,
      background: Color(0xFF24252B),
    ),
    popupMenuTheme: PopupMenuThemeData(
        shape: RoundedRectangleBorder(
      side: BorderSide(color: Colors.white24),
      borderRadius: BorderRadius.all(Radius.circular(8.0)),
    )),
  ).copyWith(
    extensions: <ThemeExtension<dynamic>>[
      ColorThemeExtension.dark,
      TabbarTheme.dark,
    ],
  );

  static ThemeMode getThemeModePreference() {
    return themeModeFromString(bind.mainGetLocalOption(key: kCommConfKeyTheme));
  }

  static Future<void> changeDarkMode(ThemeMode mode) async {
    Get.changeThemeMode(mode);
    if (desktopType == DesktopType.main || isAndroid || isIOS || isWeb) {
      if (mode == ThemeMode.system) {
        await bind.mainSetLocalOption(
            key: kCommConfKeyTheme, value: defaultOptionTheme);
      } else {
        await bind.mainSetLocalOption(
            key: kCommConfKeyTheme, value: mode.toShortString());
      }
      if (!isWeb) await bind.mainChangeTheme(dark: mode.toShortString());
      // Synchronize the window theme of the system.
      updateSystemWindowTheme();
    }
  }

  static ThemeMode currentThemeMode() {
    final preference = getThemeModePreference();
    if (preference == ThemeMode.system) {
      if (WidgetsBinding.instance.platformDispatcher.platformBrightness ==
          Brightness.light) {
        return ThemeMode.light;
      } else {
        return ThemeMode.dark;
      }
    } else {
      return preference;
    }
  }

  static ColorThemeExtension color(BuildContext context) {
    return Theme.of(context).extension<ColorThemeExtension>()!;
  }

  static TabbarTheme tabbar(BuildContext context) {
    return Theme.of(context).extension<TabbarTheme>()!;
  }

  static ThemeMode themeModeFromString(String v) {
    switch (v) {
      case "light":
        return ThemeMode.light;
      case "dark":
        return ThemeMode.dark;
      default:
        return ThemeMode.system;
    }
  }
}

extension ParseToString on ThemeMode {
  String toShortString() {
    return toString().split('.').last;
  }
}

final ButtonStyle flatButtonStyle = TextButton.styleFrom(
  minimumSize: Size(0, 36),
  padding: EdgeInsets.symmetric(horizontal: 16.0, vertical: 10.0),
  shape: const RoundedRectangleBorder(
    borderRadius: BorderRadius.all(Radius.circular(2.0)),
  ),
);

List<Locale> supportedLocales = const [
  Locale('en', 'US'),
  Locale('zh', 'CN'),
  Locale('zh', 'TW'),
  Locale('zh', 'SG'),
  Locale('fr'),
  Locale('de'),
  Locale('it'),
  Locale('ja'),
  Locale('cs'),
  Locale('pl'),
  Locale('ko'),
  Locale('hu'),
  Locale('pt'),
  Locale('ru'),
  Locale('sk'),
  Locale('id'),
  Locale('da'),
  Locale('eo'),
  Locale('tr'),
  Locale('kz'),
  Locale('es'),
  Locale('nl'),
  Locale('nb'),
  Locale('et'),
  Locale('eu'),
  Locale('bg'),
  Locale('be'),
  Locale('vn'),
  Locale('uk'),
  Locale('fa'),
  Locale('ca'),
  Locale('el'),
  Locale('sv'),
  Locale('sq'),
  Locale('sr'),
  Locale('th'),
  Locale('sl'),
  Locale('ro'),
  Locale('lt'),
  Locale('lv'),
  Locale('ar'),
  Locale('he'),
  Locale('hr'),
];

String formatDurationToTime(Duration duration) {
  var totalTime = duration.inSeconds;
  final secs = totalTime % 60;
  totalTime = (totalTime - secs) ~/ 60;
  final mins = totalTime % 60;
  totalTime = (totalTime - mins) ~/ 60;
  return "${totalTime.toString().padLeft(2, "0")}:${mins.toString().padLeft(2, "0")}:${secs.toString().padLeft(2, "0")}";
}

closeConnection({String? id}) {
  if (isDisplayPanel) {
    closeDisplayPanel(reason: kPanelCloseConn);
    return;
  }
  if (isAndroid || isIOS) {
    () async {
      await SystemChrome.setEnabledSystemUIMode(SystemUiMode.manual,
          overlays: SystemUiOverlay.values);
      gFFI.chatModel.hideChatOverlay();
      Navigator.popUntil(globalKey.currentContext!, ModalRoute.withName("/"));
      stateGlobal.isInMainPage = true;
    }();
  } else {
    if (isWeb) {
      Navigator.popUntil(globalKey.currentContext!, ModalRoute.withName("/"));
      stateGlobal.isInMainPage = true;
    } else {
      final controller = Get.find<DesktopTabController>();
      if (controller.tabType == DesktopTabType.terminal &&
          controller.onCloseWindow != null) {
        // Terminal windows are scoped to one peer. The optional id passed to
        // closeConnection() is that peer id, not a terminal tab key
        // (${peerId}_${terminalId}). Closing from terminal dialogs should close
        // the peer's whole terminal window, including all terminal tabs.
        unawaited(controller.onCloseWindow!().catchError((e, _) {
          debugPrint('[closeConnection] Failed to close terminal window: $e');
        }));
        return;
      }
      controller.closeBy(id);
    }
  }
}

Future<void> windowOnTop(int? id) async {
  if (!isDesktop) {
    return;
  }
  print("Bring window '$id' on top");
  if (id == null) {
    // main window
    if (stateGlobal.isMinimized) {
      await windowManager.restore();
    }
    await windowManager.show();
    await windowManager.focus();
    await rustDeskWinManager.registerActiveWindow(kWindowMainId);
  } else {
    WindowController.fromWindowId(id)
      ..focus()
      ..show();
    rustDeskWinManager.call(WindowType.Main, kWindowEventShow, {"id": id});
  }
}

typedef DialogBuilder = CustomAlertDialog Function(
    StateSetter setState, void Function([dynamic]) close, BuildContext context);

class Dialog<T> {
  OverlayEntry? entry;
  Completer<T?> completer = Completer<T?>();

  Dialog();

  void complete(T? res) {
    try {
      if (!completer.isCompleted) {
        completer.complete(res);
      }
    } catch (e) {
      debugPrint("Dialog complete catch error: $e");
    } finally {
      entry?.remove();
    }
  }
}

class OverlayKeyState {
  final _overlayKey = GlobalKey<OverlayState>();

  /// use global overlay by default
  OverlayState? get state =>
      _overlayKey.currentState ?? globalKey.currentState?.overlay;

  GlobalKey<OverlayState>? get key => _overlayKey;
}

class OverlayDialogManager {
  final Map<String, Dialog> _dialogs = {};
  var _overlayKeyState = OverlayKeyState();
  int _tagCount = 0;

  OverlayEntry? _mobileActionsOverlayEntry;
  RxBool mobileActionsOverlayVisible = true.obs;

  setMobileActionsOverlayVisible(bool v, {store = true}) {
    if (store) {
      bind.setLocalFlutterOption(k: kOptionShowMobileAction, v: v ? 'Y' : 'N');
    }
    // No need to read the value from local storage after setting it.
    // It better to toggle the value directly.
    mobileActionsOverlayVisible.value = v;
  }

  loadMobileActionsOverlayVisible() {
    mobileActionsOverlayVisible.value =
        bind.getLocalFlutterOption(k: kOptionShowMobileAction) != 'N';
  }

  void setOverlayState(OverlayKeyState overlayKeyState) {
    _overlayKeyState = overlayKeyState;
  }

  void dismissAll() {
    _dialogs.forEach((key, value) {
      value.complete(null);
      BackButtonInterceptor.removeByName(key);
    });
    _dialogs.clear();
  }

  void dismissByTag(String tag) {
    _dialogs[tag]?.complete(null);
    _dialogs.remove(tag);
    BackButtonInterceptor.removeByName(tag);
  }

  Future<T?> show<T>(DialogBuilder builder,
      {bool clickMaskDismiss = false,
      bool backDismiss = false,
      String? tag,
      bool useAnimation = true,
      bool forceGlobal = false}) {
    final overlayState =
        forceGlobal ? globalKey.currentState?.overlay : _overlayKeyState.state;

    if (overlayState == null) {
      return Future.error(
          "[OverlayDialogManager] Failed to show dialog, _overlayState is null, call [setOverlayState] first");
    }

    final String dialogTag;
    if (tag != null) {
      dialogTag = tag;
    } else {
      dialogTag = _tagCount.toString();
      _tagCount++;
    }

    final dialog = Dialog<T>();
    _dialogs[dialogTag] = dialog;

    close([res]) {
      _dialogs.remove(dialogTag);
      try {
        dialog.complete(res);
      } catch (e) {
        debugPrint("Dialog complete catch error: $e");
      }
      BackButtonInterceptor.removeByName(dialogTag);
    }

    dialog.entry = OverlayEntry(builder: (context) {
      bool innerClicked = false;
      return Listener(
          onPointerUp: (_) {
            if (!innerClicked && clickMaskDismiss) {
              close();
            }
            innerClicked = false;
          },
          child: Container(
              color: Theme.of(context).brightness == Brightness.light
                  ? Colors.black12
                  : Colors.black45,
              child: StatefulBuilder(builder: (context, setState) {
                return Listener(
                  onPointerUp: (_) => innerClicked = true,
                  child: builder(setState, close, overlayState.context),
                );
              })));
    });
    overlayState.insert(dialog.entry!);
    BackButtonInterceptor.add((stopDefaultButtonEvent, routeInfo) {
      if (backDismiss) {
        close();
      }
      return true;
    }, name: dialogTag);
    return dialog.completer.future;
  }

  String showLoading(String text,
      {bool clickMaskDismiss = false,
      bool showCancel = true,
      VoidCallback? onCancel,
      String? tag}) {
    if (tag == null) {
      tag = _tagCount.toString();
      _tagCount++;
    }
    show((setState, close, context) {
      cancel() {
        dismissAll();
        if (onCancel != null) {
          onCancel();
        }
      }

      return CustomAlertDialog(
        content: Container(
            constraints: const BoxConstraints(maxWidth: 240),
            child: Column(
                mainAxisSize: MainAxisSize.min,
                crossAxisAlignment: CrossAxisAlignment.start,
                children: [
                  const SizedBox(height: 30),
                  const Center(child: CircularProgressIndicator()),
                  const SizedBox(height: 20),
                  Center(
                      child: Text(translate(text),
                          style: const TextStyle(fontSize: 15))),
                  const SizedBox(height: 20),
                  Offstage(
                      offstage: !showCancel,
                      child: Center(
                          child: (isDesktop || isWebDesktop)
                              ? dialogButton('Cancel', onPressed: cancel)
                              : TextButton(
                                  style: flatButtonStyle,
                                  onPressed: cancel,
                                  child: Text(translate('Cancel'),
                                      style: const TextStyle(
                                          color: MyTheme.accent)))))
                ])),
        onCancel: showCancel ? cancel : null,
      );
    }, tag: tag);
    return tag;
  }

  void resetMobileActionsOverlay({FFI? ffi}) {
    if (_mobileActionsOverlayEntry == null) return;
    hideMobileActionsOverlay();
    showMobileActionsOverlay(ffi: ffi);
  }

  void showMobileActionsOverlay({FFI? ffi}) {
    if (_mobileActionsOverlayEntry != null) return;
    final overlayState = _overlayKeyState.state;
    if (overlayState == null) return;

    final overlay = makeMobileActionsOverlayEntry(
      () => hideMobileActionsOverlay(),
      ffi: ffi,
    );
    overlayState.insert(overlay);
    _mobileActionsOverlayEntry = overlay;
    setMobileActionsOverlayVisible(true);
  }

  void hideMobileActionsOverlay({store = true}) {
    if (_mobileActionsOverlayEntry != null) {
      _mobileActionsOverlayEntry!.remove();
      _mobileActionsOverlayEntry = null;
      setMobileActionsOverlayVisible(false, store: store);
      return;
    }
  }

  void toggleMobileActionsOverlay({FFI? ffi}) {
    if (_mobileActionsOverlayEntry == null) {
      showMobileActionsOverlay(ffi: ffi);
    } else {
      hideMobileActionsOverlay();
    }
  }

  bool existing(String tag) {
    return _dialogs.keys.contains(tag);
  }
}

makeMobileActionsOverlayEntry(VoidCallback? onHide, {FFI? ffi}) {
  makeMobileActions(BuildContext context, double s) {
    final scale = s < 0.85 ? 0.85 : s;
    final session = ffi ?? gFFI;
    const double overlayW = 200;
    const double overlayH = 45;
    computeOverlayPosition() {
      final screenW = MediaQuery.of(context).size.width;
      final screenH = MediaQuery.of(context).size.height;
      final left = (screenW - overlayW * scale) / 2;
      final top = screenH - (overlayH + 80) * scale;
      return Offset(left, top);
    }

    if (draggablePositions.mobileActions.isInvalid()) {
      draggablePositions.mobileActions.update(computeOverlayPosition());
    } else {
      draggablePositions.mobileActions.tryAdjust(overlayW, overlayH, scale);
    }
    return DraggableMobileActions(
      scale: scale,
      position: draggablePositions.mobileActions,
      width: overlayW,
      height: overlayH,
      onBackPressed: session.inputModel.onMobileBack,
      onHomePressed: session.inputModel.onMobileHome,
      onRecentPressed: session.inputModel.onMobileApps,
      onHidePressed: onHide,
    );
  }

  return OverlayEntry(builder: (context) {
    if (isDesktop) {
      final c = Provider.of<CanvasModel>(context);
      return makeMobileActions(context, c.scale * 2.0);
    } else {
      return makeMobileActions(globalKey.currentContext!, 1.0);
    }
  });
}

void showToast(String text,
    {Duration timeout = const Duration(seconds: 3),
    Alignment alignment = const Alignment(0.0, 0.8)}) {
  final overlayState = globalKey.currentState?.overlay;
  if (overlayState == null) return;
  final entry = OverlayEntry(builder: (context) {
    return IgnorePointer(
        child: Align(
            alignment: alignment,
            child: Container(
              decoration: BoxDecoration(
                color: MyTheme.color(context).toastBg,
                borderRadius: const BorderRadius.all(
                  Radius.circular(20),
                ),
              ),
              padding: const EdgeInsets.symmetric(horizontal: 20, vertical: 5),
              child: Text(
                text,
                textAlign: TextAlign.center,
                style: TextStyle(
                    decoration: TextDecoration.none,
                    fontWeight: FontWeight.w300,
                    fontSize: 18,
                    color: MyTheme.color(context).toastText),
              ),
            )));
  });
  overlayState.insert(entry);
  Future.delayed(timeout, () {
    entry.remove();
  });
}

// TODO
// - Remove argument "contentPadding", no need for it, all should look the same.
// - Remove "required" for argument "content". See simple confirm dialog "delete peer", only title and actions are used. No need to "content: SizedBox.shrink()".
// - Make dead code alive, transform arguments "onSubmit" and "onCancel" into correspondenting buttons "ConfirmOkButton", "CancelButton".
class CustomAlertDialog extends StatelessWidget {
  const CustomAlertDialog(
      {Key? key,
      this.title,
      this.titlePadding,
      required this.content,
      this.actions,
      this.contentPadding,
      this.contentBoxConstraints = const BoxConstraints(maxWidth: 500),
      this.onSubmit,
      this.onCancel})
      : super(key: key);

  final Widget? title;
  final EdgeInsetsGeometry? titlePadding;
  final Widget content;
  final List<Widget>? actions;
  final double? contentPadding;
  final BoxConstraints contentBoxConstraints;
  final Function()? onSubmit;
  final Function()? onCancel;

  @override
  Widget build(BuildContext context) {
    // request focus
    FocusScopeNode scopeNode = FocusScopeNode();
    Future.delayed(Duration.zero, () {
      if (!scopeNode.hasFocus) scopeNode.requestFocus();
    });
    bool tabTapped = false;
    if (isAndroid) gFFI.invokeMethod("enable_soft_keyboard", true);

    return FocusScope(
      node: scopeNode,
      autofocus: true,
      onKey: (node, key) {
        if (key.logicalKey == LogicalKeyboardKey.escape) {
          if (key is RawKeyDownEvent) {
            onCancel?.call();
          }
          return KeyEventResult.handled; // avoid TextField exception on escape
        } else if (!tabTapped &&
            onSubmit != null &&
            (key.logicalKey == LogicalKeyboardKey.enter ||
                key.logicalKey == LogicalKeyboardKey.numpadEnter)) {
          if (key is RawKeyDownEvent) onSubmit?.call();
          return KeyEventResult.handled;
        } else if (key.logicalKey == LogicalKeyboardKey.tab) {
          if (key is RawKeyDownEvent) {
            scopeNode.nextFocus();
            tabTapped = true;
          }
          return KeyEventResult.handled;
        }
        return KeyEventResult.ignored;
      },
      child: AlertDialog(
          scrollable: true,
          title: title,
          content: ConstrainedBox(
            constraints: contentBoxConstraints,
            child: content,
          ),
          actions: actions,
          titlePadding: titlePadding ?? MyTheme.dialogTitlePadding(),
          contentPadding:
              MyTheme.dialogContentPadding(actions: actions is List),
          actionsPadding: MyTheme.dialogActionsPadding(),
          buttonPadding: MyTheme.dialogButtonPadding),
    );
  }
}

Widget createDialogContent(String text) {
  final RegExp linkRegExp = RegExp(r'(https?://[^\s]+)');
  bool hasLink = linkRegExp.hasMatch(text);

  // Early return: no link, use default theme color
  if (!hasLink) {
    return SelectableText(text, style: const TextStyle(fontSize: 15));
  }

  final List<TextSpan> spans = [];
  int start = 0;

  linkRegExp.allMatches(text).forEach((match) {
    if (match.start > start) {
      spans.add(TextSpan(text: text.substring(start, match.start)));
    }
    spans.add(TextSpan(
      text: match.group(0) ?? '',
      style: const TextStyle(
        color: Colors.blue,
        decoration: TextDecoration.underline,
      ),
      recognizer: TapGestureRecognizer()
        ..onTap = () {
          String linkText = match.group(0) ?? '';
          linkText = linkText.replaceAll(RegExp(r'[.,;!?]+$'), '');
          launchUrl(Uri.parse(linkText));
        },
    ));
    start = match.end;
  });

  if (start < text.length) {
    spans.add(TextSpan(text: text.substring(start)));
  }

  return SelectableText.rich(
    TextSpan(
      style: const TextStyle(fontSize: 15),
      children: spans,
    ),
  );
}

void msgBox(SessionID sessionId, String type, String title, String text,
    String link, OverlayDialogManager dialogManager,
    {bool? hasCancel,
    ReconnectHandle? reconnect,
    int? reconnectTimeout,
    VoidCallback? onSubmit,
    int? submitTimeout}) {
  dialogManager.dismissAll();
  if (type.contains('insecure-connection')) {
    Future<void> closeSession() async {
      await bind.sessionSetCommon(
        sessionId: sessionId,
        key: 'continue-insecure-connection',
        value: 'N',
      );
      dialogManager.dismissAll();
      closeConnection();
    }

    void continueSession() {
      unawaited(
        bind.sessionSetCommon(
          sessionId: sessionId,
          key: 'continue-insecure-connection',
          value: 'Y',
        ),
      );
      dialogManager.dismissAll();
    }

    dialogManager.show(
      (setState, close, context) => CustomAlertDialog(
        title: null,
        content: SelectionArea(child: msgboxContent(type, title, text)),
        actions: [
          dialogButton(
            'Continue',
            onPressed: continueSession,
            isOutline: true,
          ),
          dialogButton('Disconnect', onPressed: closeSession),
        ],
        onSubmit: closeSession,
        onCancel: closeSession,
      ),
      tag: '$sessionId-$type-$title-$text-$link',
    );
    return;
  }

  List<Widget> buttons = [];
  bool hasOk = false;
  submit() {
    dialogManager.dismissAll();
    if (onSubmit != null) {
      onSubmit.call();
    } else {
      // https://github.com/rustdesk/rustdesk/blob/5e9a31340b899822090a3731769ae79c6bf5f3e5/src/ui/common.tis#L263
      if (!type.contains("custom") && desktopType != DesktopType.portForward) {
        closeConnection();
      }
    }
  }

  cancel() {
    dialogManager.dismissAll();
  }

  jumplink() {
    if (link.startsWith('http')) {
      launchUrl(Uri.parse(link));
    }
  }

  if (type != "connecting" && type != "success" && !type.contains("nook")) {
    hasOk = true;
    late final Widget btn;
    if (submitTimeout != null) {
      btn = _CountDownButton(
        text: 'OK',
        second: submitTimeout,
        onPressed: submit,
        submitOnTimeout: true,
      );
    } else {
      btn = dialogButton('OK', onPressed: submit);
    }
    buttons.insert(0, btn);
  }
  hasCancel ??= !type.contains("error") &&
      !type.contains("nocancel") &&
      type != "restarting";
  if (hasCancel) {
    buttons.insert(
        0, dialogButton('Cancel', onPressed: cancel, isOutline: true));
  }
  if (type.contains("hasclose")) {
    buttons.insert(
        0,
        dialogButton('Close', onPressed: () {
          dialogManager.dismissAll();
        }));
  }
  if (reconnect != null &&
      title == "Connection Error" &&
      reconnectTimeout != null) {
    // `enabled` is used to disable the dialog button once the button is clicked.
    final enabled = true.obs;
    final button = Obx(() => _CountDownButton(
          text: 'Reconnect',
          second: reconnectTimeout,
          onPressed: enabled.isTrue
              ? () {
                  // Disable the button
                  enabled.value = false;
                  reconnect(dialogManager, sessionId, false);
                }
              : null,
        ));
    buttons.insert(0, button);
  }
  if (link.isNotEmpty) {
    buttons.insert(0, dialogButton('JumpLink', onPressed: jumplink));
  }
  dialogManager.show(
    (setState, close, context) => CustomAlertDialog(
      title: null,
      content: SelectionArea(child: msgboxContent(type, title, text)),
      actions: buttons,
      onSubmit: hasOk ? submit : null,
      onCancel: hasCancel == true ? cancel : null,
    ),
    tag: '$sessionId-$type-$title-$text-$link',
  );
}

Color? _msgboxColor(String type) {
  if (type == "input-password" || type == "custom-os-password") {
    return Color(0xFFAD448E);
  }
  if (type.contains("success")) {
    return Color(0xFF32bea6);
  }
  if (type.contains("error") || type == "re-input-password") {
    return Color(0xFFE04F5F);
  }
  return Color(0xFF2C8CFF);
}

Widget msgboxIcon(String type) {
  IconData? iconData;
  if (type.contains("error") || type == "re-input-password") {
    iconData = Icons.cancel;
  }
  if (type.contains("success")) {
    iconData = Icons.check_circle;
  }
  if (type == "wait-uac" || type == "wait-remote-accept-nook") {
    iconData = Icons.hourglass_top;
  }
  if (type == 'on-uac' || type == 'on-foreground-elevated') {
    iconData = Icons.admin_panel_settings;
  }
  if (type.contains('info')) {
    iconData = Icons.info;
  }
  if (iconData != null) {
    return Icon(iconData, size: 50, color: _msgboxColor(type))
        .marginOnly(right: 16);
  }

  return Offstage();
}

// title should be null
Widget msgboxContent(String type, String title, String text) {
  String translateText(String text) {
    if (text.indexOf('Failed') == 0 && text.indexOf(': ') > 0) {
      List<String> words = text.split(': ');
      for (var i = 0; i < words.length; ++i) {
        words[i] = translate(words[i]);
      }
      text = words.join(': ');
    } else {
      List<String> words = text.split(' ');
      if (words.length > 1 && words[0].endsWith('_tip')) {
        words[0] = translate(words[0]);
        final rest = text.substring(words[0].length + 1);
        text = '${words[0]} ${translate(rest)}';
      } else {
        text = translate(text);
      }
    }
    return text;
  }

  return Row(
    children: [
      msgboxIcon(type),
      Expanded(
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Text(
              translate(title),
              style: TextStyle(fontSize: 21),
            ).marginOnly(bottom: 10),
            createDialogContent(translateText(text)),
          ],
        ),
      ),
    ],
  ).marginOnly(bottom: 12);
}

void msgBoxCommon(OverlayDialogManager dialogManager, String title,
    Widget content, List<Widget> buttons,
    {bool hasCancel = true}) {
  dialogManager.show((setState, close, context) => CustomAlertDialog(
        title: Text(
          translate(title),
          style: TextStyle(fontSize: 21),
        ),
        content: content,
        actions: buttons,
        onCancel: hasCancel ? close : null,
      ));
}

Color str2color(String str, [alpha = 0xFF]) {
  var hash = 160 << 16 + 114 << 8 + 91;
  for (var i = 0; i < str.length; i += 1) {
    hash = str.codeUnitAt(i) + ((hash << 5) - hash);
  }
  hash = hash % 16777216;
  return Color((hash & 0xFF7FFF) | (alpha << 24));
}

Color str2color2(String str, {List<int> existing = const []}) {
  Map<String, Color> colorMap = {
    "red": Colors.red,
    "green": Colors.green,
    "blue": Colors.blue,
    "orange": Colors.orange,
    "purple": Colors.purple,
    "grey": Colors.grey,
    "cyan": Colors.cyan,
    "lime": Colors.lime,
    "teal": Colors.teal,
    "pink": Colors.pink[200]!,
    "indigo": Colors.indigo,
    "brown": Colors.brown,
  };
  final color = colorMap[str.toLowerCase()];
  if (color != null) {
    return color.withAlpha(0xFF);
  }
  if (str.toLowerCase() == 'yellow') {
    return Colors.yellow.withAlpha(0xFF);
  }
  var hash = 0;
  for (var i = 0; i < str.length; i++) {
    hash += str.codeUnitAt(i);
  }
  List<Color> colorList = colorMap.values.toList();
  hash = hash % colorList.length;
  var result = colorList[hash].withAlpha(0xFF);
  if (existing.contains(result.value)) {
    Color? notUsed =
        colorList.firstWhereOrNull((e) => !existing.contains(e.value));
    if (notUsed != null) {
      result = notUsed;
    }
  }
  return result;
}

const K = 1024;
const M = K * K;
const G = M * K;

String readableFileSize(double size) {
  if (size < K) {
    return "${size.toStringAsFixed(2)} B";
  } else if (size < M) {
    return "${(size / K).toStringAsFixed(2)} KB";
  } else if (size < G) {
    return "${(size / M).toStringAsFixed(2)} MB";
  } else {
    return "${(size / G).toStringAsFixed(2)} GB";
  }
}

/// Flutter can't not catch PointerMoveEvent when size is 1
/// This will happen in Android AccessibilityService Input
/// android can't init dispatching size yet ,see: https://stackoverflow.com/questions/59960451/android-accessibility-dispatchgesture-is-it-possible-to-specify-pressure-for-a
/// use this temporary solution until flutter or android fixes the bug
class AccessibilityListener extends StatelessWidget {
  final Widget? child;
  static final offset = 100;

  AccessibilityListener({this.child});

  @override
  Widget build(BuildContext context) {
    return Listener(
        onPointerDown: (evt) {
          if (evt.size == 1) {
            GestureBinding.instance.handlePointerEvent(PointerAddedEvent(
                pointer: evt.pointer + offset, position: evt.position));
            GestureBinding.instance.handlePointerEvent(PointerDownEvent(
                pointer: evt.pointer + offset,
                size: 0.1,
                position: evt.position));
          }
        },
        onPointerUp: (evt) {
          if (evt.size == 1) {
            GestureBinding.instance.handlePointerEvent(PointerUpEvent(
                pointer: evt.pointer + offset,
                size: 0.1,
                position: evt.position));
            GestureBinding.instance.handlePointerEvent(PointerRemovedEvent(
                pointer: evt.pointer + offset, position: evt.position));
          }
        },
        onPointerMove: (evt) {
          if (evt.size == 1) {
            GestureBinding.instance.handlePointerEvent(PointerMoveEvent(
                pointer: evt.pointer + offset,
                size: 0.1,
                delta: evt.delta,
                position: evt.position));
          }
        },
        child: child);
  }
}

class AndroidPermissionManager {
  static Completer<bool>? _completer;
  static Timer? _timer;
  static var _current = "";

  static Future<bool> check(String type) {
    if (isDesktop || isWeb) {
      return Future.value(true);
    }
    return gFFI.invokeMethod("check_permission", type);
  }

  // startActivity goto Android Setting's page to request permission manually by user
  static void startAction(String action) {
    gFFI.invokeMethod(AndroidChannel.kStartAction, action);
  }

  /// We use XXPermissions to request permissions,
  /// for supported types, see https://github.com/getActivity/XXPermissions/blob/e46caea32a64ad7819df62d448fb1c825481cd28/library/src/main/java/com/hjq/permissions/Permission.java
  static Future<bool> request(String type) {
    if (isDesktop || isWeb) {
      return Future.value(true);
    }

    gFFI.invokeMethod("request_permission", type);

    // clear last task
    if (_completer?.isCompleted == false) {
      _completer?.complete(false);
    }
    _timer?.cancel();

    _current = type;
    _completer = Completer<bool>();

    _timer = Timer(Duration(seconds: 120), () {
      if (_completer == null) return;
      if (!_completer!.isCompleted) {
        _completer!.complete(false);
      }
      _completer = null;
      _current = "";
    });
    return _completer!.future;
  }

  static complete(String type, bool res) {
    if (type != _current) {
      res = false;
    }
    _timer?.cancel();
    _completer?.complete(res);
    _current = "";
  }
}

RadioListTile<T> getRadio<T>(
    Widget title, T toValue, T curValue, ValueChanged<T?>? onChange,
    {bool? dense}) {
  return RadioListTile<T>(
    visualDensity: VisualDensity.compact,
    controlAffinity: ListTileControlAffinity.trailing,
    title: title,
    value: toValue,
    groupValue: curValue,
    onChanged: onChange,
    dense: dense,
  );
}

/// find ffi, tag is Remote ID
/// for session specific usage
FFI ffi(String? tag) {
  return Get.find<FFI>(tag: tag);
}

/// Global FFI object
late FFI _globalFFI;

FFI get gFFI => _globalFFI;

/// Android: set in an engine that runs as an extra display panel (DisplayActivity):
/// {id, sid, display, cache}.
Map<String, dynamic>? kDisplayPanelArgs;
bool get isDisplayPanel => kDisplayPanelArgs != null;

/// Why a panel closes itself, passed on to the main panel ([AndroidDisplayPanels.onPanelClosed]).
const kPanelCloseUser = 'user'; // the user closed it: its display may go
const kPanelCloseGone = 'gone'; // its display left the host's list
const kPanelCloseConn = 'conn'; // the session or connection ended

/// Extra display panel: detach this panel's UI session and close its window. The connection
/// stays: it belongs to the main panel.
Future<void> closeDisplayPanel({String reason = kPanelCloseUser}) async {
  try {
    await bind
        .sessionClose(sessionId: gFFI.sessionId)
        .timeout(const Duration(seconds: 1));
  } catch (e) {
    debugPrint('panel: sessionClose failed: $e');
  }
  platformFFI.invokeMethod('panel_finish', {'reason': reason});
}

/// Main engine only (Android): the extra Quest panels, one per remote display. Each is a
/// second UI session on this connection, shown by its own activity and Flutter engine.
///
/// Two rules keep it sound: a display has one reader (the main window shows one display, the
/// panels the others), and nobody switches displays while panels are open (a switch replaces
/// the host's capture set), so every switch closes the panels first.
///
/// The session starts with the main window alone. "+" gives one more display a window (a new
/// virtual display if every display has one), "-" takes the last one away again.
///
/// Two kinds of host: Amyuni (only the display added last can be unplugged, so displays go
/// from the end of the list) and the RustDesk IDD of this fork ([isIddMode]: every virtual
/// display has an index 1..4 and is plugged in and out on its own, so a closed window takes
/// exactly its own display away).
class AndroidDisplayPanels {
  static const _kSessionKey = 'quest-display-session';
  static const _kAutoOrderKey = 'quest-panel-auto-order';

  final _open = <int, String>{}; // display -> the panel's UI session id
  // RustDesk IDD: display -> which display the panel was opened for ([_identity]), to find
  // it again when the host's list shifts (a display in the middle went or came).
  final _openId = <int, String>{};
  // RustDesk IDD indices this session plugged in, oldest first ("-" takes the newest).
  final _iddOrder = <int>[];
  // RustDesk IDD indices asked for and not plugged in yet -> when.
  final _iddAsked = <int, int>{};
  // RustDesk IDD indices asked for ("+", restore) whose display has no window yet -> when
  // asked. [_resync] opens it once the host lists the index (it may come in the middle of
  // the host's list, so not by position).
  final _iddFresh = <int, int>{};
  // Displays whose window went while the host's list changed: they get one again.
  final _orphans = <String>[];
  Timer? _resyncTimer;
  final isOpenRx = false.obs;

  /// Bumped on every change, for the toolbar and the overview.
  final revision = 0.obs;
  bool _busy = false;
  // Displays asked from the host ("+", session restore) that have not arrived yet.
  int _pendingAdds = 0;
  int _addAskedMs = 0;
  // When this connection's first picture came ([onFirstImage]), and a count of connections
  // so that a plug-in held for the host ([_whenHostTakesPlug]) never reaches the next one.
  int _firstImageMs = 0;
  int _connection = 0;
  // Left-to-right order to apply once every pending display has its window.
  List<int>? _orderAfterAdds;
  int _lastAutoOrderMs = 0;

  /// The restore question was asked on this connection.
  bool restoreOffered = false;

  /// Quest game mode (XrActivity) is on: the panels sit behind the immersive view and the
  /// system may close them. Until it ends, a closed panel never unplugs its display (no
  /// [trimTrailing] / [_trimIdd], no unplug in [onPanelClosed]); the displays whose panel
  /// went are recorded and get one again when game mode ends ([onGameModeExited]).
  bool gameModeSuspended = false;
  // [_identity] of each display whose panel closed during game mode.
  final _gameModeClosed = <String>[];

  bool get isOpen => _open.isNotEmpty;
  bool isPanelOpen(int display) => _open.containsKey(display);
  bool get _waitingForDisplay =>
      _pendingAdds > 0 &&
      DateTime.now().millisecondsSinceEpoch - _addAskedMs < 30000;

  /// Follow the laser: when it goes from one window straight into another, the host's
  /// displays are put in that order.
  bool get autoOrder => bind.mainGetLocalOption(key: _kAutoOrderKey) != 'N';
  set autoOrder(bool v) {
    bind.mainSetLocalOption(key: _kAutoOrderKey, value: v ? '' : 'N');
    revision.value++;
  }

  /// A new connection starts: nothing is open, nothing is pending.
  void newConnection() {
    _open.clear();
    isOpenRx.value = false;
    _pendingAdds = 0;
    _firstImageMs = 0;
    _connection++;
    _orderAfterAdds = null;
    restoreOffered = false;
    _openId.clear();
    _iddOrder.clear();
    _iddAsked.clear();
    _iddFresh.clear();
    _orphans.clear();
    _resyncTimer?.cancel();
    gameModeSuspended = false;
    _gameModeClosed.clear();
    revision.value++;
  }

  /// Quest game mode is about to start: the main view shows one display (switching from
  /// "all displays" if needed), and panels stop unplugging displays. Returns that display.
  Future<int> enterGameMode(FFI ffi) async {
    final pi = ffi.ffiModel.pi;
    var d = pi.currentDisplay;
    if (d == kAllDisplayValue || d < 0 || d >= pi.displays.length) {
      d = mainDisplay(ffi);
      _openMonitorInTheSameTab(d, ffi, pi);
      await Future.delayed(const Duration(milliseconds: 300));
    }
    gameModeSuspended = true;
    _gameModeClosed.clear();
    debugPrint('[panel] game mode on: display $d, ${_open.length} panels');
    return d;
  }

  /// Quest game mode ended ([reason] from the XR side): displays may be unplugged again, and
  /// the panels the system closed meanwhile are reopened.
  Future<void> onGameModeExited(FFI ffi, String reason) async {
    if (!gameModeSuspended) return;
    gameModeSuspended = false;
    final ids = List<String>.of(_gameModeClosed);
    _gameModeClosed.clear();
    if (ffi.closed || isDisplayPanel) return;
    final displays = <int>[];
    for (final id in ids) {
      final d = _findIdentity(ffi, id);
      if (d >= 0 && !_open.containsKey(d) && !displays.contains(d)) displays.add(d);
    }
    debugPrint('[panel] game mode over ($reason): reopen displays $displays');
    if (displays.isNotEmpty) await _openPanels(ffi, displays);
    Future.delayed(const Duration(seconds: 2), () {
      if (!ffi.closed) trimTrailing(ffi);
    });
  }

  /// The display the main window shows.
  int mainDisplay(FFI ffi) {
    final pi = ffi.ffiModel.pi;
    var d = pi.currentDisplay;
    if (d < 0 || d >= pi.displays.length) d = pi.primaryDisplay;
    if (d < 0 || d >= pi.displays.length) d = 0;
    return d;
  }

  /// Display indices from left to right, as the host has them.
  List<int> order(FFI ffi) {
    final ds = ffi.ffiModel.pi.displays;
    final l = List.generate(ds.length, (i) => i);
    l.sort((a, b) {
      final c = ds[a].x.compareTo(ds[b].x);
      return c != 0 ? c : a.compareTo(b);
    });
    return l;
  }

  /// The host has the RustDesk IDD backend and unplugs exactly the index it is given.
  bool isIddMode(FFI ffi) => ffi.ffiModel.pi.isRustDeskIddTargeted;

  /// RustDesk IDD index of [display], null when it is not one of its virtual displays.
  /// By the Windows device name (DisplayInfo.name against virtual_display_names). Without
  /// names, only a guess that cannot be wrong in the usual case: the host plugged its
  /// virtual displays in ascending index order and lists them last; otherwise null (no
  /// index: a close then unplugs nothing).
  int? iddIndexOf(FFI ffi, int display) {
    final pi = ffi.ffiModel.pi;
    if (display < 0 || display >= pi.displays.length) return null;
    final hostOrder = pi.RustDeskVirtualDisplays;
    if (hostOrder.isEmpty) return null;
    final plugged = [...hostOrder]..sort();
    final name = pi.displays[display].name.toUpperCase();
    final names = pi.virtualDisplayNames;
    if (name.isNotEmpty && names.isNotEmpty) {
      for (final e in names.entries) {
        if (e.value == name && plugged.contains(e.key)) return e.key;
      }
      return null;
    }
    // Plugged out of index order (an index below one in use was taken again): the host's
    // list order of the displays is not known.
    for (var i = 0; i < plugged.length; i++) {
      if (plugged[i] != hostOrder[i]) return null;
    }
    final first = pi.displays.length - plugged.length;
    if (first < 0 || display < first) return null;
    return plugged[display - first];
  }

  /// The display that shows RustDesk IDD index [idx], if any.
  int? displayOfIdd(FFI ffi, int idx) {
    for (var d = 0; d < ffi.ffiModel.pi.displays.length; d++) {
      if (iddIndexOf(ffi, d) == idx) return d;
    }
    return null;
  }

  /// RustDesk IDD: the index to unplug to take [display] away, null when it must stay (not
  /// a virtual display, the main display, or left of it: the main window's display would
  /// shift).
  int? removableIddIndex(FFI ffi, int display) {
    if (!isIddMode(ffi)) return null;
    final m = ffi.ffiModel;
    if (!m.keyboard || m.viewOnly) return null;
    final keep = mainDisplay(ffi);
    if (display == keep || display < keep) return null;
    return iddIndexOf(ffi, display);
  }

  /// Overview: whether "remove" takes the display itself away, not only its window.
  bool removesDisplay(FFI ffi, int display) => isIddMode(ffi)
      ? removableIddIndex(ffi, display) != null
      : display == ffi.ffiModel.pi.displays.length - 1;

  // Which display a panel shows, in a form that survives a shift of the host's list.
  String _identity(FFI ffi, int d) {
    final pi = ffi.ffiModel.pi;
    if (d < 0 || d >= pi.displays.length) return '';
    final n = pi.displays[d].name;
    if (n.isNotEmpty) return 'n:${n.toUpperCase()}';
    final idx = iddIndexOf(ffi, d);
    return idx != null ? 'i:$idx' : 'd:$d';
  }

  int _findIdentity(FFI ffi, String id) {
    final pi = ffi.ffiModel.pi;
    if (id.length < 3) return -1;
    final v = id.substring(2);
    switch (id.substring(0, 2)) {
      case 'n:':
        return pi.displays.indexWhere((x) => x.name.toUpperCase() == v);
      case 'i:':
        final idx = int.tryParse(v);
        return idx == null ? -1 : (displayOfIdd(ffi, idx) ?? -1);
      case 'd:':
        final d = int.tryParse(v) ?? -1;
        return d < pi.displays.length ? d : -1;
    }
    return -1;
  }

  // The next RustDesk IDD index to plug in: above every index in use (the new display
  // then comes last in the host's list), else the lowest free one.
  int? _freeIddIndex(FFI ffi) {
    final plugged = ffi.ffiModel.pi.RustDeskVirtualDisplays;
    final now = DateTime.now().millisecondsSinceEpoch;
    _iddAsked
        .removeWhere((i, t) => plugged.contains(i) || now - t > 30000);
    final taken = {...plugged, ..._iddAsked.keys};
    var top = 0;
    for (final i in taken) {
      if (i > top) top = i;
    }
    for (var i = top + 1; i <= kMaxVirtualDisplayCount; i++) {
      if (!taken.contains(i)) return i;
    }
    for (var i = 1; i <= kMaxVirtualDisplayCount; i++) {
      if (!taken.contains(i)) return i;
    }
    return null;
  }

  void _unplugIdd(FFI ffi, int idx) {
    if (!ffi.ffiModel.keyboard || ffi.ffiModel.viewOnly) return;
    _iddOrder.remove(idx);
    _iddAsked.remove(idx);
    _iddFresh.remove(idx);
    debugPrint('[panel] unplug virtual display (index $idx)');
    toggleAmyuniVirtualDisplay(ffi, false, index: idx);
  }

  // The display "-" takes away: the virtual display this session plugged in last, else
  // the host's highest one.
  int? _newestIddDisplay(FFI ffi) {
    final plugged = ffi.ffiModel.pi.RustDeskVirtualDisplays;
    final rest = plugged.where((i) => !_iddOrder.contains(i)).toList()
      ..sort((a, b) => b.compareTo(a));
    for (final idx in [
      ..._iddOrder.reversed.where(plugged.contains),
      ...rest,
    ]) {
      final d = displayOfIdd(ffi, idx);
      if (d != null && removableIddIndex(ffi, d) == idx) return d;
    }
    return null;
  }

  bool canPlug(FFI ffi) {
    final m = ffi.ffiModel;
    if (isIddMode(ffi)) {
      return showVirtualDisplayMenu(ffi) &&
          m.keyboard &&
          !m.viewOnly &&
          _freeIddIndex(ffi) != null &&
          m.pi.displays.length < 4;
    }
    return m.pi.isAmyuniIdd &&
        showVirtualDisplayMenu(ffi) &&
        m.keyboard &&
        !m.viewOnly &&
        m.pi.amyuniVirtualDisplayCount < 4 &&
        m.pi.displays.length < 4;
  }

  void _changed(FFI ffi, {bool save = true}) {
    _poses.removeWhere((id, _) => id >= 0 && !_open.containsKey(id));
    _lastLayoutSent = '';
    isOpenRx.value = isOpen;
    revision.value++;
    if (save) _save(ffi);
  }

  // What to offer at the next connection: how many displays had a window, and their order.
  void _save(FFI ffi) {
    if (ffi.id.isEmpty) return;
    try {
      bind.mainSetPeerOptionSync(
          id: ffi.id,
          key: _kSessionKey,
          value: _open.isEmpty
              ? ''
              : jsonEncode({
                  'n': _open.length,
                  'open': (_open.keys.toList()..sort()),
                  'order': order(ffi),
                }));
    } catch (e) {
      debugPrint('[panel] save failed: $e');
    }
  }

  Future<void> _openPanels(FFI ffi, List<int> displays) async {
    if (!isAndroid || isDisplayPanel || _busy || displays.isEmpty) return;
    final pi = ffi.ffiModel.pi;
    _busy = true;
    try {
      // The core's key for the connection can differ from ffi.id (id@server, relay suffix).
      if (bind.peerGetSessionsCount(id: ffi.id, connType: ffi.connType.index) <
          1) {
        showToast('Separate windows are not available for this connection');
        return;
      }
      var keep = pi.currentDisplay;
      if (keep == kAllDisplayValue) {
        keep = mainDisplay(ffi);
        _openMonitorInTheSameTab(keep, ffi, pi);
        await Future.delayed(const Duration(milliseconds: 300));
      }
      // With more than one UI session the core sends a display's frames only to a session
      // that lists it: make sure the main view's display is on its list (no-op if it is).
      if (keep >= 0 && keep < pi.displays.length) {
        await bind.sessionSetSize(
            sessionId: ffi.sessionId,
            display: keep,
            width: pi.displays[keep].width,
            height: pi.displays[keep].height);
      }
      // Frames of the other displays that still reach the main view must not be shown.
      ffi.imageModel.dropOtherDisplays();
      final cache = ffi.ffiModel.cachedPeerData.toString();
      for (final i in displays) {
        if (i == keep ||
            i < 0 ||
            i >= pi.displays.length ||
            _open.containsKey(i)) {
          continue;
        }
        final sid = const Uuid().v4();
        _open[i] = sid;
        _openId[i] = _identity(ffi, i);
        isOpenRx.value = true;
        final ok = await platformFFI.invokeMethod('panel_open', {
          'display': i,
          'sid': sid,
          'args': jsonEncode({
            'id': ffi.id,
            'sid': sid,
            'display': i,
            'cache': cache,
            // The host announces this once per connection, before any panel exists.
            'hostAnswersFocus': ffi.inputModel.quest.hostAnswersFocus,
          }),
        });
        debugPrint('[panel] open display $i: $ok');
        if (ok != true && _open[i] == sid) {
          _open.remove(i);
          _openId.remove(i);
        }
      }
    } catch (e) {
      debugPrint('[panel] open failed: $e');
    } finally {
      _busy = false;
      _changed(ffi);
    }
  }

  /// A window for one display that has none.
  Future<void> openPanel(FFI ffi, int display) => _openPanels(ffi, [display]);

  void _requestPlug(FFI ffi) {
    _addAskedMs = DateTime.now().millisecondsSinceEpoch;
    if (isIddMode(ffi)) {
      final idx = _freeIddIndex(ffi);
      if (idx == null) {
        _pendingAdds = 0;
        _orderAfterAdds = null;
        showToast('No more displays can be added');
        return;
      }
      _iddAsked[idx] = _addAskedMs;
      _iddFresh[idx] = _addAskedMs;
      _iddOrder
        ..remove(idx)
        ..add(idx);
      _whenHostTakesPlug(ffi, () {
        // The 30 s waits ([_waitingForDisplay], [_iddAsked], [_iddFresh]) start at the send.
        _addAskedMs = DateTime.now().millisecondsSinceEpoch;
        if (_iddAsked.containsKey(idx)) _iddAsked[idx] = _addAskedMs;
        if (_iddFresh.containsKey(idx)) _iddFresh[idx] = _addAskedMs;
        debugPrint(
            '[panel] ask the host for virtual display $idx ($_pendingAdds pending)');
        toggleAmyuniVirtualDisplay(ffi, true, index: idx);
      });
      return;
    }
    debugPrint('[panel] ask the host for one more display ($_pendingAdds pending)');
    toggleAmyuniVirtualDisplay(ffi, true);
  }

  /// The main session's first picture of this connection arrived (FFI.onEvent2UIRgba).
  void onFirstImage() {
    if (!isDisplayPanel) _firstImageMs = DateTime.now().millisecondsSinceEpoch;
  }

  // A RustDesk IDD host drops a plug-in that comes within 3 s of login or of its first frame
  // to this viewer (it takes it for a viewer replaying its remembered displays,
  // src/server/connection.rs toggle_virtual_display), and says nothing. So a "+" or restore
  // that early is held until 3.5 s after the first picture, which comes after both.
  void _whenHostTakesPlug(FFI ffi, void Function() send) {
    final conn = _connection;
    void attempt() {
      if (ffi.closed || conn != _connection) return;
      if (ffi.ffiModel.waitForFirstImage.isTrue) {
        Future.delayed(const Duration(milliseconds: 300), attempt);
        return;
      }
      final wait = _firstImageMs == 0
          ? 0
          : _firstImageMs + 3500 - DateTime.now().millisecondsSinceEpoch;
      if (wait > 0) {
        debugPrint('[panel] virtual display request held ${wait}ms for the host');
        Future.delayed(Duration(milliseconds: wait), attempt);
        return;
      }
      send();
    }

    attempt();
  }

  /// "+": one more display in a window of its own. A display without a window gets one;
  /// otherwise the host is asked for a new virtual display, which gets its window when it
  /// arrives ([onDisplaysGrown]).
  Future<void> addOne(FFI ffi) async {
    if (!isAndroid || isDisplayPanel || _busy || _waitingForDisplay) return;
    final pi = ffi.ffiModel.pi;
    final keep = mainDisplay(ffi);
    for (var i = 0; i < pi.displays.length; i++) {
      if (i != keep && !_open.containsKey(i)) {
        await _openPanels(ffi, [i]);
        return;
      }
    }
    if (!canPlug(ffi)) {
      showToast('No more displays can be added');
      return;
    }
    _pendingAdds = 1;
    _requestPlug(ffi);
  }

  /// The host's display list grew (FfiModel.handleSyncPeerInfo).
  void onDisplaysGrown(FFI ffi, int oldCount, int newCount) {
    if (isIddMode(ffi)) {
      // A display that came in the middle of the list shifts the ones after it, and the new
      // one need not be at the end: [_resync] finds it by its index ([_iddFresh]) once the
      // host's platform additions list it (they may come after the display list), then
      // goes on with the pending adds ([_continueAdds]).
      if (_waitingForDisplay) {
        _pendingAdds -= newCount - oldCount;
        if (_pendingAdds < 0) _pendingAdds = 0;
      } else {
        _pendingAdds = 0;
      }
      _scheduleResync(ffi);
      return;
    }
    if (!_waitingForDisplay) {
      _pendingAdds = 0;
      return;
    }
    final fresh = [for (var i = oldCount; i < newCount; i++) i];
    _pendingAdds -= fresh.length;
    if (_pendingAdds < 0) _pendingAdds = 0;
    () async {
      await _openPanels(ffi, fresh);
      await _continueAdds(ffi);
    }();
  }

  /// RustDesk IDD: the host's platform additions changed (plugged indices, their names).
  void onIddAdditionsChanged(FFI ffi) {
    if (!isAndroid || isDisplayPanel || !isIddMode(ffi)) return;
    if (_iddFresh.isNotEmpty || isOpen) _scheduleResync(ffi);
  }

  // After the windows for newly arrived displays are open: the next pending add, or the
  // order to apply once every one has arrived.
  Future<void> _continueAdds(FFI ffi) async {
    if (_pendingAdds > 0) {
      if (canPlug(ffi)) {
        _requestPlug(ffi);
        return;
      }
      _pendingAdds = 0;
    }
    final o = _orderAfterAdds;
    _orderAfterAdds = null;
    if (o != null) {
      // The host is still settling its new display.
      await Future.delayed(const Duration(milliseconds: 1500));
      if (!ffi.closed) applyOrder(ffi, o);
    }
  }

  void _expireIddFresh() {
    final now = DateTime.now().millisecondsSinceEpoch;
    _iddFresh.removeWhere((_, t) => now - t > 30000);
  }

  /// The host's display list shrank: the windows of the displays that went away go too.
  void onDisplaysShrunk(FFI ffi, int newCount) {
    if (isIddMode(ffi)) {
      // Any display can go, also one in the middle: the windows follow their displays
      // ([_resync]); the windows of the displays that went away go too.
      _scheduleResync(ffi);
      Future.delayed(const Duration(milliseconds: 1700), () {
        if (!ffi.closed) trimTrailing(ffi);
      });
      return;
    }
    for (final d in _open.keys.where((d) => d >= newCount).toList()) {
      closePanel(ffi, d);
    }
    // The next windowless display at the end, if any.
    Future.delayed(const Duration(milliseconds: 1700), () {
      if (!ffi.closed) trimTrailing(ffi);
    });
  }

  /// Closes one display's window. The display itself stays on the host.
  Future<void> closePanel(FFI ffi, int display) async {
    final sid = _open.remove(display);
    _openId.remove(display);
    if (sid == null) return;
    debugPrint('[panel] close display $display');
    _changed(ffi);
    try {
      await bind.sessionClose(sessionId: UuidValue(sid));
      await bind.setCurSessionId(sessionId: gFFI.sessionId);
    } catch (e) {
      debugPrint('[panel] close: $e');
    }
    platformFFI.invokeMethod('panel_close', sid);
  }

  /// Takes [display] away: its window, and the display itself when it is the host's last
  /// virtual one (the driver can only remove the one added last).
  Future<void> removeDisplay(FFI ffi, int display) async {
    final pi = ffi.ffiModel.pi;
    if (display == mainDisplay(ffi)) return;
    if (isIddMode(ffi)) {
      // RustDesk IDD: exactly this display goes (if it is a virtual one).
      final idx = removableIddIndex(ffi, display);
      await closePanel(ffi, display);
      if (idx != null) _unplugIdd(ffi, idx);
      return;
    }
    await closePanel(ffi, display);
    trimTrailing(ffi);
  }

  int _lastTrimMs = 0;

  /// A virtual display exists only while it has a window: every display without one slows
  /// the host's capture of the others. The host's last display is unplugged when it has no
  /// window (the driver can only remove the one added last); the shorter list brings this
  /// here again for the next one ([onDisplaysShrunk]).
  void trimTrailing(FFI ffi) {
    if (!isAndroid || isDisplayPanel || _busy || _waitingForDisplay) return;
    // Game mode: the panels are hidden, not gone (see [gameModeSuspended]).
    if (gameModeSuspended) return;
    if (isIddMode(ffi)) {
      _trimIdd(ffi);
      return;
    }
    final pi = ffi.ffiModel.pi;
    final last = pi.displays.length - 1;
    if (last < 1 || last == mainDisplay(ffi) || _open.containsKey(last)) return;
    if (!pi.isAmyuniIdd ||
        pi.amyuniVirtualDisplayCount < 1 ||
        !ffi.ffiModel.keyboard ||
        ffi.ffiModel.viewOnly) {
      return;
    }
    final now = DateTime.now().millisecondsSinceEpoch;
    if (now - _lastTrimMs < 1500) return;
    _lastTrimMs = now;
    debugPrint('[panel] display ${last + 1} has no window: ask the host to remove it');
    toggleAmyuniVirtualDisplay(ffi, false);
  }

  /// RustDesk IDD: a virtual display this session plugged in that has no window goes
  /// (one per call; the shorter list brings this here again). Displays plugged in by
  /// someone else stay.
  void _trimIdd(FFI ffi) {
    if (gameModeSuspended) return;
    // Windows are still being moved to their displays.
    if (_resyncing || _resyncTimer?.isActive == true || _orphans.isNotEmpty) {
      return;
    }
    _expireIddFresh();
    final plugged = ffi.ffiModel.pi.RustDeskVirtualDisplays;
    for (final idx in _iddOrder.reversed.toList()) {
      // Asked for and not given its window yet ([_resync]).
      if (!plugged.contains(idx) || _iddFresh.containsKey(idx)) continue;
      final d = displayOfIdd(ffi, idx);
      if (d == null ||
          _open.containsKey(d) ||
          removableIddIndex(ffi, d) != idx) {
        continue;
      }
      final now = DateTime.now().millisecondsSinceEpoch;
      if (now - _lastTrimMs < 1500) return;
      _lastTrimMs = now;
      debugPrint('[panel] display ${d + 1} has no window: unplug it');
      _unplugIdd(ffi, idx);
      return;
    }
  }

  /// "-": the display added last goes.
  Future<void> removeLast(FFI ffi) async {
    if (!isAndroid || isDisplayPanel || _busy) return;
    final pi = ffi.ffiModel.pi;
    final keep = mainDisplay(ffi);
    if (isIddMode(ffi)) {
      final d = _newestIddDisplay(ffi);
      if (d != null) {
        await removeDisplay(ffi, d);
        return;
      }
      // No virtual display to unplug: the last window closes.
      final open = _open.keys.toList()..sort();
      if (open.isNotEmpty) {
        await removeDisplay(ffi, open.last);
        return;
      }
      showToast('Nothing to remove');
      return;
    }
    final last = pi.displays.length - 1;
    for (var d = last; d >= 0; d--) {
      if (d == keep) continue;
      final unplug = d == last &&
          d > 0 &&
          pi.isAmyuniIdd &&
          pi.amyuniVirtualDisplayCount > 0 &&
          ffi.ffiModel.keyboard &&
          !ffi.ffiModel.viewOnly;
      if (!_open.containsKey(d) && !unplug) continue;
      await removeDisplay(ffi, d);
      return;
    }
    showToast('Nothing to remove');
  }

  Future<void> closeAll() async {
    if (!isOpen) return;
    final sids = _open.values.toList();
    _open.clear();
    _openId.clear();
    _orphans.clear();
    _iddFresh.clear();
    _resyncTimer?.cancel();
    isOpenRx.value = false;
    revision.value++;
    _pendingAdds = 0;
    _orderAfterAdds = null;
    debugPrint('[panel] close all (${sids.length})');
    try {
      await Future.wait(
          sids.map((s) => bind.sessionClose(sessionId: UuidValue(s))));
      await bind.setCurSessionId(sessionId: gFFI.sessionId);
    } catch (e) {
      debugPrint('[panel] closeAll: $e');
    }
    platformFFI.invokeMethod('panel_close_all');
  }

  /// The window of a panel is gone (its own close button, or the system one). [reason]: why
  /// the panel closed itself ([kPanelCloseUser], [kPanelCloseGone], [kPanelCloseConn]).
  void onPanelClosed(int display, String sid, {String reason = kPanelCloseUser}) {
    debugPrint('[panel] closed display $display ($reason)');
    bind.sessionClose(sessionId: UuidValue(sid)); // no-op if the panel did it itself
    if (_open[display] == sid) {
      _open.remove(display);
      final id = _openId.remove(display);
      if (gameModeSuspended && reason != kPanelCloseConn) {
        // Closed by the system behind the immersive view: the display stays plugged in and
        // gets its panel back when game mode ends. Not saved: the next connection should
        // still offer the panels as they were.
        _gameModeClosed.add(id ?? _identity(gFFI, display));
        debugPrint('[panel] display $display closed during game mode: reopen later');
        _changed(gFFI, save: false);
        if (gFFI.id.isNotEmpty) bind.setCurSessionId(sessionId: gFFI.sessionId);
        return;
      }
      _changed(gFFI);
      if (isIddMode(gFFI)) {
        // RustDesk IDD: exactly this window's display goes.
        _afterIddPanelClosed(gFFI, display, id ?? '', reason);
      } else {
        // Closed with its own button: the display goes with its window if it is the last.
        trimTrailing(gFFI);
      }
    }
    if (gFFI.id.isNotEmpty) bind.setCurSessionId(sessionId: gFFI.sessionId);
  }

  /// RustDesk IDD: a panel window went by itself. Closed by the user: its display (found by
  /// identity, so a shift of the host's list does not matter) is unplugged. Gone because its
  /// display left the host's list under it: the display it showed, if still there, gets a
  /// window again ([_resync]). Closed because the session ended: nothing.
  void _afterIddPanelClosed(FFI ffi, int display, String id, String reason) {
    if (ffi.closed || !isIddMode(ffi) || id.isEmpty) return;
    if (reason == kPanelCloseGone) {
      _orphans.add(id);
      _scheduleResync(ffi);
      return;
    }
    if (reason != kPanelCloseUser) return;
    final d = _findIdentity(ffi, id);
    final idx = d < 0 ? null : removableIddIndex(ffi, d);
    if (idx == null) {
      debugPrint('[panel] display ${display + 1} stays on the host');
      return;
    }
    _unplugIdd(ffi, idx);
  }

  void _scheduleResync(FFI ffi) {
    _resyncTimer?.cancel();
    // The host's display list and its list of virtual displays arrive separately.
    _resyncTimer =
        Timer(const Duration(milliseconds: 700), () => _resync(ffi));
  }

  /// RustDesk IDD: after the host's list changed, every display that had a window has one
  /// again, with as few windows closed and opened as possible. A window whose index still
  /// holds a display that should have one keeps it (it may now show another of them);
  /// the others close, and the displays left without a window get a new one.
  Future<void> _resync(FFI ffi) async {
    if (ffi.closed || !isIddMode(ffi)) {
      _orphans.clear();
      return;
    }
    if (_busy) {
      _scheduleResync(ffi);
      return;
    }
    final keep = mainDisplay(ffi);
    final want = <int>{};
    for (final id in [
      for (final e in _openId.entries)
        if (_open.containsKey(e.key)) e.value,
      ..._orphans,
    ]) {
      final j = _findIdentity(ffi, id);
      if (j >= 0 && j != keep) want.add(j);
    }
    _orphans.clear();
    // Displays asked for that the host now lists get their window, wherever they are.
    _expireIddFresh();
    final plugged = ffi.ffiModel.pi.RustDeskVirtualDisplays;
    var freshArrived = false;
    for (final idx in _iddFresh.keys.toList()) {
      if (!plugged.contains(idx)) continue;
      final d = displayOfIdd(ffi, idx);
      if (d == null) continue; // its name is not known yet
      _iddFresh.remove(idx);
      freshArrived = true;
      if (d != keep) want.add(d);
    }
    for (final k in _open.keys) {
      if (want.contains(k)) _openId[k] = _identity(ffi, k);
    }
    final close = _open.keys.where((k) => !want.contains(k)).toList();
    final open = want.where((j) => !_open.containsKey(j)).toList()..sort();
    if (close.isEmpty && open.isEmpty) {
      if (freshArrived) await _continueAdds(ffi);
      return;
    }
    debugPrint('[panel] the host\'s displays moved: close $close, open $open');
    _resyncing = true;
    try {
      for (final k in close) {
        await closePanel(ffi, k);
      }
      if (open.isNotEmpty) {
        await Future.delayed(const Duration(milliseconds: 300));
        if (!ffi.closed) await _openPanels(ffi, open);
      }
    } finally {
      _resyncing = false;
    }
    if (freshArrived && !ffi.closed) await _continueAdds(ffi);
  }

  bool _resyncing = false;

  /// Asks the host to put its displays in one row in this left-to-right order
  /// (ToggleVirtualDisplay values from 9000 on, see connection.rs on the host).
  Future<void> applyOrder(FFI ffi, List<int> order) async {
    // Only a host of this fork knows the request (it says so); any other Windows host
    // would plug in a virtual display instead.
    if (ffi.ffiModel.pi.platformAdditions['display_row_layout'] != true) {
      debugPrint('[panel] the host cannot arrange its displays');
      return;
    }
    final n = ffi.ffiModel.pi.displays.length;
    if (n < 2 ||
        n > 4 ||
        order.length != n ||
        order.toSet().length != n ||
        order.any((d) => d < 0 || d >= n)) {
      return;
    }
    var code = 0;
    for (var i = 0; i < n; i++) {
      code |= order[i] << (2 * i);
    }
    debugPrint('[panel] order $order');
    try {
      // The toggle call also edits the stored list of displays to plug in again: undo it.
      const key = 'virtual-display';
      final before =
          await bind.sessionGetPeerOption(sessionId: ffi.sessionId, name: key);
      await bind.sessionToggleVirtualDisplay(
          sessionId: ffi.sessionId, index: 9000 + n * 256 + code, on: true);
      await bind.sessionPeerOption(
          sessionId: ffi.sessionId, name: key, value: before);
    } catch (e) {
      debugPrint('[panel] order failed: $e');
    }
    // The host's new positions arrive with its next display list.
    Future.delayed(const Duration(seconds: 2), () {
      revision.value++;
      if (isOpen) _save(ffi);
    });
  }

  /// Overview: [display] one place to the left (-1) or right (+1).
  void move(FFI ffi, int display, int by) {
    final o = order(ffi);
    final i = o.indexOf(display);
    final j = i + by;
    if (i < 0 || j < 0 || j >= o.length) return;
    o[i] = o[j];
    o[j] = display;
    applyOrder(ffi, o);
  }

  // Where each window stands in space, from Horizon OS (PoseWatcher in DisplayActivity.kt):
  // -1 = the main window, else the display a panel shows -> position x,y,z (m), orientation
  // x,y,z,w, width and height (m).
  final _poses = <int, List<double>>{};
  Timer? _poseTimer;
  String _lastLayoutSent = '';

  void onPose(FFI ffi, int id, List<double> v) {
    if (v.length < 9) return;
    _poses[id] = v;
    _poseTimer?.cancel();
    _poseTimer =
        Timer(const Duration(milliseconds: 600), () => _layoutFromPoses(ffi));
  }

  /// The host's displays arranged the way their windows stand in space: the main display
  /// stays, every other one goes edge to edge against the nearest display already placed,
  /// on the side its window is on, shifted along that edge as the window is (small shifts
  /// are taken as none). This is what the system's own remote desktop app does.
  void _layoutFromPoses(FFI ffi) {
    if (!autoOrder || !isOpen || ffi.closed || _busy || _waitingForDisplay) {
      return;
    }
    final pi = ffi.ffiModel.pi;
    if (pi.platformAdditions['display_xy_layout'] != true) return;
    final n = pi.displays.length;
    if (n < 2 || n > 4) return;
    final keep = mainDisplay(ffi);
    final mp = _poses[-1];
    if (mp == null || mp[7] <= 0) return;
    // Every display needs a window whose place is known.
    for (var i = 0; i < n; i++) {
      if (i != keep && (!_open.containsKey(i) || _poses[i] == null)) return;
    }
    final ppm = pi.displays[keep].width / mp[7]; // remote pixels per metre
    // The main window's right and up directions.
    final qx = mp[3], qy = mp[4], qz = mp[5], qw = mp[6];
    final rx = 1 - 2 * (qy * qy + qz * qz);
    final ry = 2 * (qx * qy + qw * qz);
    final rz = 2 * (qx * qz - qw * qy);
    final ux = 2 * (qx * qy - qw * qz);
    final uy = 1 - 2 * (qx * qx + qz * qz);
    final uz = 2 * (qy * qz + qw * qx);
    // Centre of each window in remote pixels, the main window's centre at 0,0, y down.
    final cx = <int, double>{keep: 0};
    final cy = <int, double>{keep: 0};
    for (var i = 0; i < n; i++) {
      if (i == keep) continue;
      final p = _poses[i]!;
      final dx = p[0] - mp[0], dy = p[1] - mp[1], dz = p[2] - mp[2];
      cx[i] = (dx * rx + dy * ry + dz * rz) * ppm;
      cy[i] = -(dx * ux + dy * uy + dz * uz) * ppm;
    }
    final pos = <int, List<int>>{
      keep: [0, 0]
    };
    final rest = [
      for (var i = 0; i < n; i++)
        if (i != keep) i
    ]..sort((a, b) => (cx[a]! * cx[a]! + cy[a]! * cy[a]!)
        .compareTo(cx[b]! * cx[b]! + cy[b]! * cy[b]!));
    for (final d in rest) {
      var nb = keep;
      var best = double.infinity;
      for (final k in pos.keys) {
        final ex = cx[d]! - cx[k]!, ey = cy[d]! - cy[k]!;
        final dd = ex * ex + ey * ey;
        if (dd < best) {
          best = dd;
          nb = k;
        }
      }
      final wd = pi.displays[d].width, hd = pi.displays[d].height;
      final wn = pi.displays[nb].width, hn = pi.displays[nb].height;
      final ex = cx[d]! - cx[nb]!, ey = cy[d]! - cy[nb]!;
      int x, y;
      if (ex.abs() / (wd + wn) >= ey.abs() / (hd + hn)) {
        // Beside it: left or right, and how far up or down along that edge.
        x = pos[nb]![0] + (ex >= 0 ? wn : -wd);
        var off = (ey - (hd - hn) / 2).round();
        if (off.abs() < 60) off = 0;
        off = off.clamp(-(hd - 120), hn - 120).toInt();
        y = pos[nb]![1] + off;
      } else {
        // Above or below it.
        y = pos[nb]![1] + (ey >= 0 ? hn : -hd);
        var off = (ex - (wd - wn) / 2).round();
        if (off.abs() < 60) off = 0;
        off = off.clamp(-(wd - 120), wn - 120).toInt();
        x = pos[nb]![0] + off;
      }
      pos[d] = [x, y];
    }
    final key = [
      for (var i = 0; i < n; i++) '${pos[i]![0]},${pos[i]![1]}'
    ].join(';');
    if (key == _lastLayoutSent) return;
    _lastLayoutSent = key;
    // Already what the host has?
    final ox = pi.displays[keep].x, oy = pi.displays[keep].y;
    var same = true;
    for (var i = 0; i < n; i++) {
      if ((pi.displays[i].x - ox).round() != pos[i]![0] ||
          (pi.displays[i].y - oy).round() != pos[i]![1]) {
        same = false;
      }
    }
    debugPrint('[panel] windows in space -> displays $key${same ? ' (as it is)' : ''}');
    if (!same) _sendPositions(ffi, pos);
  }

  /// One ToggleVirtualDisplay value per display (bit 30, display index, x, y), then one that
  /// says "apply": see toggle_virtual_display in the host's connection.rs.
  Future<void> _sendPositions(FFI ffi, Map<int, List<int>> pos) async {
    for (final v in pos.values) {
      if (v[0] < -8192 || v[0] > 8191 || v[1] < -4096 || v[1] > 4095) return;
    }
    try {
      // The toggle call also edits the stored list of displays to plug in again: undo it.
      const key = 'virtual-display';
      final before =
          await bind.sessionGetPeerOption(sessionId: ffi.sessionId, name: key);
      for (final e in pos.entries) {
        await bind.sessionToggleVirtualDisplay(
            sessionId: ffi.sessionId,
            index: 0x40000000 |
                (e.key << 27) |
                ((e.value[0] + 8192) << 13) |
                (e.value[1] + 4096),
            on: true);
      }
      await bind.sessionToggleVirtualDisplay(
          sessionId: ffi.sessionId, index: 0x40000000 | (7 << 27), on: true);
      await bind.sessionPeerOption(
          sessionId: ffi.sessionId, name: key, value: before);
    } catch (e) {
      debugPrint('[panel] positions failed: $e');
    }
    Future.delayed(const Duration(seconds: 2), () {
      revision.value++;
      if (isOpen) _save(ffi);
    });
  }

  /// The laser left one window on its right edge and entered another on its left edge:
  /// [right] stands to the right of [left] (-1 is the main window). Only used when the
  /// windows' places are not known (see [onPose]).
  void onNeighbor(FFI ffi, int left, int right) {
    if (!autoOrder || !isOpen) return;
    if (_poses.containsKey(-1) &&
        ffi.ffiModel.pi.platformAdditions['display_xy_layout'] == true) {
      return;
    }
    final keep = mainDisplay(ffi);
    final l = left < 0 ? keep : left;
    final r = right < 0 ? keep : right;
    final o = order(ffi);
    final il = o.indexOf(l);
    final ir = o.indexOf(r);
    if (il < 0 || ir < 0 || l == r || ir == il + 1) return;
    final now = DateTime.now().millisecondsSinceEpoch;
    if (now - _lastAutoOrderMs < 3000) return; // the last change is still on its way
    _lastAutoOrderMs = now;
    o.removeAt(ir);
    o.insert(o.indexOf(l) + 1, r);
    debugPrint('[panel] the laser went from $l to $r');
    applyOrder(ffi, o);
  }

  /// Once per host on a Quest with hardware decoding: ask for 72 fps (the panel's
  /// refresh rate) at custom quality 200, which keeps about today's bits per frame.
  /// A host whose virtual displays run at 90 Hz or more (RustDesk IDD,
  /// virtual_display_hz) gets 90 fps instead, once more (version v2-90).
  /// Changing the quality later in the toolbar sticks: this runs only once per version.
  static const _kFpsDefaultsKey = 'quest-fps-defaults';
  static const _kFpsDefaultsVersion = 'v1-72';
  static const _kFpsDefaultsVersion90 = 'v2-90';
  Future<void> applyQuestFrameRate(FFI ffi) async {
    if (!isAndroid || isDisplayPanel) return;
    if (!ffi.inputModel.quest.isQuest.value) return;
    if (bind.mainGetLocalOption(key: kOptionEnableAndroidSurfaceDecode) == 'N') {
      return;
    }
    try {
      // Called from the peer info handler before it reads the platform additions.
      await Future.delayed(const Duration(milliseconds: 500));
      if (ffi.closed) return;
      final fps = ffi.ffiModel.pi.virtualDisplayHz >= 90 ? 90 : 72;
      final version = fps == 90 ? _kFpsDefaultsVersion90 : _kFpsDefaultsVersion;
      final stored =
          bind.mainGetPeerOptionSync(id: ffi.id, key: _kFpsDefaultsKey);
      // Once per version; a host that once got 90 is not set back to 72 when it announces
      // no rate (another backend): the user's own choice since then stays.
      if (stored == version ||
          (fps == 72 && stored == _kFpsDefaultsVersion90)) {
        return;
      }
      final before =
          await bind.sessionGetImageQuality(sessionId: ffi.sessionId);
      await bind.sessionSetImageQuality(
          sessionId: ffi.sessionId, value: kRemoteImageQualityCustom);
      await bind.sessionSetCustomImageQuality(
          sessionId: ffi.sessionId, value: 200);
      await bind.sessionSetCustomFps(sessionId: ffi.sessionId, fps: fps);
      await bind.mainSetPeerOption(
          id: ffi.id, key: _kFpsDefaultsKey, value: version);
      debugPrint('[quest] frame rate: custom quality 200, $fps fps (was $before)');
    } catch (e) {
      debugPrint('[quest] frame rate defaults failed: $e');
    }
  }

  /// At connect: a session with more windows was used on this host last time.
  void maybeOfferRestore(FFI ffi) {
    if (!isAndroid || isDisplayPanel || restoreOffered) return;
    if (!ffi.inputModel.quest.isQuest.value) return;
    restoreOffered = true;
    int n = 0;
    List<int>? ord;
    List<int>? open;
    try {
      final raw = bind.mainGetPeerOptionSync(id: ffi.id, key: _kSessionKey);
      if (raw.isEmpty) return;
      final saved = jsonDecode(raw) as Map<String, dynamic>;
      n = (saved['n'] as int?) ?? 0;
      ord = (saved['order'] as List?)?.map((e) => e as int).toList();
      open = (saved['open'] as List?)?.map((e) => e as int).toList();
    } catch (e) {
      debugPrint('[panel] saved session unreadable: $e');
      return;
    }
    if (n < 1) {
      // No session to bring back: displays left over on the host go.
      Future.delayed(const Duration(seconds: 4), () {
        if (!ffi.closed) trimTrailing(ffi);
      });
      return;
    }
    final count = n.clamp(1, 3);
    void ask() {
      if (ffi.closed || isOpen) return;
      // The first picture closes every dialog: ask after it.
      if (ffi.ffiModel.waitForFirstImage.isTrue) {
        Future.delayed(const Duration(milliseconds: 300), ask);
        return;
      }
      ffi.dialogManager.show((setState, close, context) {
        return CustomAlertDialog(
          title: const Text('Előző munkamenet'),
          content: Text(
              'Legutóbb $count további kijelző volt külön ablakban ezen a gépen. '
              'Visszaállítsam?'),
          actions: [
            dialogButton('Nem', onPressed: () {
              close();
              bind.mainSetPeerOptionSync(
                  id: ffi.id, key: _kSessionKey, value: '');
              trimTrailing(ffi);
            }, isOutline: true),
            dialogButton('Igen', onPressed: () {
              close();
              restore(ffi, count, ord, open);
            }),
          ],
        );
      });
    }

    Future.delayed(const Duration(milliseconds: 1500), ask);
  }

  /// Opens [n] more displays in windows of their own (asking the host for the ones it does
  /// not have), then puts the displays in the saved order.
  Future<void> restore(FFI ffi, int n, List<int>? ord,
      [List<int>? open]) async {
    final pi = ffi.ffiModel.pi;
    final keep = mainDisplay(ffi);
    // The displays that had a window last time first, then any other.
    final wanted = [
      for (final i in open ?? const <int>[])
        if (i != keep &&
            i >= 0 &&
            i < pi.displays.length &&
            !_open.containsKey(i))
          i
    ];
    final existing = [
      ...wanted,
      for (var i = 0; i < pi.displays.length; i++)
        if (i != keep && !_open.containsKey(i) && !wanted.contains(i)) i
    ].take(n).toList();
    debugPrint('[panel] restore $n displays (order $ord), here already: $existing');
    await _openPanels(ffi, existing);
    final missing = n - existing.length;
    if (missing > 0 && canPlug(ffi)) {
      _orderAfterAdds = ord;
      _pendingAdds = missing;
      _requestPlug(ffi);
    } else if (ord != null) {
      applyOrder(ffi, ord);
    }
  }
}

final androidDisplayPanels = AndroidDisplayPanels();

Future<void> initGlobalFFI([SessionID? sessionId]) async {
  debugPrint("_globalFFI init");
  _globalFFI = FFI(sessionId);
  debugPrint("_globalFFI init end");
  // after `put`, can also be globally found by Get.find<FFI>();
  Get.put<FFI>(_globalFFI, permanent: true);
}

String translate(String name) {
  if (name.startsWith('Failed to') && name.contains(': ')) {
    return name.split(': ').map((x) => translate(x)).join(': ');
  }
  return platformFFI.translate(name, localeName);
}

// This function must be kept the same as the one in rust and sciter code.
// rust: libs/hbb_common/src/config.rs -> option2bool()
// sciter: Does not have the function, but it should be kept the same.
bool option2bool(String option, String value) {
  bool res;
  if (option.startsWith("enable-")) {
    res = value != "N";
  } else if (option.startsWith("allow-") ||
      option == kOptionStopService ||
      option == kOptionDirectServer ||
      option == kOptionForceAlwaysRelay) {
    res = value == "Y";
  } else {
    // "" is true
    res = value != "N";
  }
  return res;
}

String bool2option(String option, bool b) {
  String res;
  if (option.startsWith('enable-') &&
      option != kOptionEnableUdpPunch &&
      option != kOptionEnableIpv6Punch &&
      option != kOptionEnableWebrtc) {
    res = b ? defaultOptionYes : 'N';
  } else if (option.startsWith('allow-') ||
      option == kOptionStopService ||
      option == kOptionDirectServer ||
      option == kOptionForceAlwaysRelay) {
    res = b ? 'Y' : defaultOptionNo;
  } else {
    res = b ? 'Y' : 'N';
  }
  return res;
}

mainSetBoolOption(String key, bool value) async {
  String v = bool2option(key, value);
  await bind.mainSetOption(key: key, value: v);
}

Future<bool> mainGetBoolOption(String key) async {
  return option2bool(key, await bind.mainGetOption(key: key));
}

bool mainGetBoolOptionSync(String key) {
  return option2bool(key, bind.mainGetOptionSync(key: key));
}

mainSetLocalBoolOption(String key, bool value) async {
  String v = bool2option(key, value);
  await bind.mainSetLocalOption(key: key, value: v);
}

bool mainGetLocalBoolOptionSync(String key) {
  return option2bool(key, bind.mainGetLocalOption(key: key));
}

bool mainGetPeerBoolOptionSync(String id, String key) {
  return option2bool(key, bind.mainGetPeerOptionSync(id: id, key: key));
}

// Don't use `option2bool()` and `bool2option()` to convert the session option.
// Use `sessionGetToggleOption()` and `sessionToggleOption()` instead.
// Because all session options use `Y` and `<Empty>` as values.

Future<bool> matchPeer(
    String searchText, Peer peer, PeerTabIndex peerTabIndex) async {
  if (searchText.isEmpty) {
    return true;
  }
  if (peer.id.toLowerCase().contains(searchText)) {
    return true;
  }
  if (peer.hostname.toLowerCase().contains(searchText) ||
      peer.username.toLowerCase().contains(searchText)) {
    return true;
  }
  if (peer.alias.toLowerCase().contains(searchText)) {
    return true;
  }
  if (peerTabShowNote(peerTabIndex) &&
      peer.note.toLowerCase().contains(searchText)) {
    return true;
  }
  return false;
}

/// Get the image for the current [platform].
Widget getPlatformImage(String platform, {double size = 50}) {
  if (platform.isEmpty) {
    return Container(width: size, height: size);
  }
  if (platform == kPeerPlatformMacOS) {
    platform = 'mac';
  } else if (platform != kPeerPlatformLinux &&
      platform != kPeerPlatformAndroid) {
    platform = 'win';
  } else {
    platform = platform.toLowerCase();
  }
  return SvgPicture.asset('assets/$platform.svg', height: size, width: size);
}

class LastWindowPosition {
  double? width;
  double? height;
  double? offsetWidth;
  double? offsetHeight;
  bool? isMaximized;
  bool? isFullscreen;

  LastWindowPosition(this.width, this.height, this.offsetWidth,
      this.offsetHeight, this.isMaximized, this.isFullscreen);

  bool equals(LastWindowPosition other) {
    return ((width == other.width) &&
        (height == other.height) &&
        (offsetWidth == other.offsetWidth) &&
        (offsetHeight == other.offsetHeight) &&
        (isMaximized == other.isMaximized) &&
        (isFullscreen == other.isFullscreen));
  }

  Map<String, dynamic> toJson() {
    return <String, dynamic>{
      "width": width,
      "height": height,
      "offsetWidth": offsetWidth,
      "offsetHeight": offsetHeight,
      "isMaximized": isMaximized,
      "isFullscreen": isFullscreen,
    };
  }

  @override
  String toString() {
    return jsonEncode(toJson());
  }

  static LastWindowPosition? loadFromString(String content) {
    if (content.isEmpty) {
      return null;
    }
    try {
      final m = jsonDecode(content);
      return LastWindowPosition(m["width"], m["height"], m["offsetWidth"],
          m["offsetHeight"], m["isMaximized"], m["isFullscreen"]);
    } catch (e) {
      debugPrintStack(
          label:
              'Failed to load LastWindowPosition "$content" ${e.toString()}');
      return null;
    }
  }
}

String get windowFramePrefix =>
    kWindowPrefix +
    (bind.isIncomingOnly()
        ? "incoming_"
        : (bind.isOutgoingOnly() ? "outgoing_" : ""));

typedef WindowKey = ({WindowType type, int? windowId});

LastWindowPosition? _lastWindowPosition = null;
final Debouncer _saveWindowDebounce = Debouncer(delay: Duration(seconds: 1));

/// Save window position and size on exit
/// Note that windowId must be provided if it's subwindow
Future<void> saveWindowPosition(WindowType type,
    {int? windowId, bool? flush}) async {
  if (type != WindowType.Main && windowId == null) {
    debugPrint(
        "Error: windowId cannot be null when saving positions for sub window");
  }

  Offset? position;
  Size? sz;
  late bool isMaximized;
  bool isFullscreen = stateGlobal.fullscreen.isTrue;

  setPreFrame() {
    final pos = bind.getLocalFlutterOption(k: windowFramePrefix + type.name);
    var lpos = LastWindowPosition.loadFromString(pos);
    if (lpos != null) {
      if (lpos.offsetWidth != null && lpos.offsetHeight != null) {
        position = Offset(lpos.offsetWidth!, lpos.offsetHeight!);
      }
      if (lpos.width != null && lpos.height != null) {
        sz = Size(lpos.width!, lpos.height!);
      }
    }
  }

  switch (type) {
    case WindowType.Main:
      // Checking `bind.isIncomingOnly()` is a simple workaround for MacOS.
      // `await windowManager.isMaximized()` will always return true
      // if is not resizable. The reason is unknown.
      //
      // `setResizable(!bind.isIncomingOnly());` in main.dart
      isMaximized =
          bind.isIncomingOnly() ? false : await windowManager.isMaximized();
      if (isFullscreen || isMaximized) {
        setPreFrame();
      } else {
        position = await windowManager.getPosition(
            ignoreDevicePixelRatio: _ignoreDevicePixelRatio);
        sz = await windowManager.getSize(
            ignoreDevicePixelRatio: _ignoreDevicePixelRatio);
      }
      break;
    default:
      final wc = WindowController.fromWindowId(windowId!);
      isMaximized = await wc.isMaximized();
      if (isFullscreen || isMaximized) {
        setPreFrame();
      } else {
        final Rect frame;
        try {
          frame = await wc.getFrame();
        } catch (e) {
          debugPrint(
              "Failed to get frame of window $windowId, it may be hidden");
          return;
        }
        position = frame.topLeft;
        sz = frame.size;
      }
      break;
  }
  if (isWindows && position != null) {
    const kMinOffset = -10000;
    const kMaxOffset = 10000;
    if (position!.dx < kMinOffset ||
        position!.dy < kMinOffset ||
        position!.dx > kMaxOffset ||
        position!.dy > kMaxOffset) {
      debugPrint("Invalid position: $position, ignore saving position");
      return;
    }
  }

  final pos = LastWindowPosition(sz?.width, sz?.height, position?.dx,
      position?.dy, isMaximized, isFullscreen);

  final WindowKey key = (type: type, windowId: windowId);

  final bool haveNewWindowPosition =
      (_lastWindowPosition == null) || !pos.equals(_lastWindowPosition!);
  final bool isPreviousNewWindowPositionPending = _saveWindowDebounce.isRunning;

  if (haveNewWindowPosition || isPreviousNewWindowPositionPending) {
    _lastWindowPosition = pos;

    if (flush ?? false) {
      // If a previous update is pending, replace it.
      _saveWindowDebounce.cancel();
      await _saveWindowPositionActual(key);
    } else if (haveNewWindowPosition) {
      _saveWindowDebounce.call(() => _saveWindowPositionActual(key));
    }
  }
}

Future<void> _saveWindowPositionActual(WindowKey key) async {
  LastWindowPosition? pos = _lastWindowPosition;

  if (pos != null) {
    debugPrint(
        "Saving frame: ${key.windowId}: ${pos.width}/${pos.height}, offset:${pos.offsetWidth}/${pos.offsetHeight}, isMaximized:${pos.isMaximized}, isFullscreen:${pos.isFullscreen}");

    await bind.setLocalFlutterOption(
        k: windowFramePrefix + key.type.name, v: pos.toString());

    if ((key.type == WindowType.RemoteDesktop ||
            key.type == WindowType.ViewCamera) &&
        key.windowId != null) {
      await _saveSessionWindowPosition(key.type, key.windowId!,
          pos.isMaximized ?? false, pos.isFullscreen ?? false, pos);
    }
  }
}

Future _saveSessionWindowPosition(WindowType windowType, int windowId,
    bool isMaximized, bool isFullscreen, LastWindowPosition pos) async {
  final remoteList = await DesktopMultiWindow.invokeMethod(
      windowId, kWindowEventGetRemoteList, null);
  getPeerPos(String peerId) {
    if (isMaximized || isFullscreen) {
      final peerPos = bind.mainGetPeerFlutterOptionSync(
          id: peerId, k: windowFramePrefix + windowType.name);
      var lpos = LastWindowPosition.loadFromString(peerPos);
      return LastWindowPosition(
              lpos?.width ?? pos.offsetWidth,
              lpos?.height ?? pos.offsetHeight,
              lpos?.offsetWidth ?? pos.offsetWidth,
              lpos?.offsetHeight ?? pos.offsetHeight,
              isMaximized,
              isFullscreen)
          .toString();
    } else {
      return pos.toString();
    }
  }

  if (remoteList != null) {
    for (final peerId in remoteList.split(',')) {
      bind.mainSetPeerFlutterOptionSync(
          id: peerId,
          k: windowFramePrefix + windowType.name,
          v: getPeerPos(peerId));
    }
  }
}

Future<Size> _adjustRestoreMainWindowSize(double? width, double? height) async {
  const double minWidth = 1;
  const double minHeight = 1;
  const double maxWidth = 6480;
  const double maxHeight = 6480;

  final defaultWidth =
      ((isDesktop || isWebDesktop) ? 1280 : kMobileDefaultDisplayWidth)
          .toDouble();
  final defaultHeight =
      ((isDesktop || isWebDesktop) ? 720 : kMobileDefaultDisplayHeight)
          .toDouble();
  double restoreWidth = width ?? defaultWidth;
  double restoreHeight = height ?? defaultHeight;

  if (restoreWidth < minWidth) {
    restoreWidth = defaultWidth;
  }
  if (restoreHeight < minHeight) {
    restoreHeight = defaultHeight;
  }
  if (restoreWidth > maxWidth) {
    restoreWidth = defaultWidth;
  }
  if (restoreHeight > maxHeight) {
    restoreHeight = defaultHeight;
  }
  return Size(restoreWidth, restoreHeight);
}

// Consider using Rect.contains() instead,
// though the implementation is not exactly the same.
bool isPointInRect(Offset point, Rect rect) {
  return point.dx >= rect.left &&
      point.dx <= rect.right &&
      point.dy >= rect.top &&
      point.dy <= rect.bottom;
}

/// return null means center
Future<Offset?> _adjustRestoreMainWindowOffset(
  double? left,
  double? top,
  double? width,
  double? height,
) async {
  if (left == null || top == null || width == null || height == null) {
    return null;
  }

  if (isDesktop || isWebDesktop) {
    final screens = await window_size.getScreenList();
    if (screens.isNotEmpty) {
      final windowRect = Rect.fromLTWH(left, top, width, height);
      bool isVisible = false;
      for (final screen in screens) {
        final intersection = windowRect.intersect(screen.visibleFrame);
        if (intersection.width >= 10.0 && intersection.height >= 10.0) {
          isVisible = true;
          break;
        }
      }
      if (!isVisible) {
        return null;
      }
      return Offset(left, top);
    }
  }

  double frameLeft = 0.0;
  double frameTop = 0.0;
  double frameRight = ((isDesktop || isWebDesktop)
          ? kDesktopMaxDisplaySize
          : kMobileMaxDisplaySize)
      .toDouble();
  double frameBottom = ((isDesktop || isWebDesktop)
          ? kDesktopMaxDisplaySize
          : kMobileMaxDisplaySize)
      .toDouble();

  final minWidth = 10.0;
  if ((left + minWidth) > frameRight ||
      (top + minWidth) > frameBottom ||
      (left + width - minWidth) < frameLeft ||
      top < frameTop) {
    return null;
  } else {
    return Offset(left, top);
  }
}

/// Restore window position and size on start
/// Note that windowId must be provided if it's subwindow
//
// display is used to set the offset of the window in individual display mode.
Future<bool> restoreWindowPosition(WindowType type,
    {int? windowId, String? peerId, int? display}) async {
  if (bind
      .mainGetEnv(key: "DISABLE_RUSTDESK_RESTORE_WINDOW_POSITION")
      .isNotEmpty) {
    return false;
  }
  if (type != WindowType.Main && windowId == null) {
    debugPrint(
        "Error: windowId cannot be null when saving positions for sub window");
    return false;
  }

  bool isRemotePeerPos = false;
  String? pos;
  // No need to check mainGetLocalBoolOptionSync(kOptionOpenNewConnInTabs)
  // Though "open in tabs" is true and the new window restore peer position, it's ok.
  if ((type == WindowType.RemoteDesktop || type == WindowType.ViewCamera) &&
      windowId != null &&
      peerId != null) {
    final peerPos = bind.mainGetPeerFlutterOptionSync(
        id: peerId, k: windowFramePrefix + type.name);
    if (peerPos.isNotEmpty) {
      pos = peerPos;
    }
    isRemotePeerPos = pos != null;
  }
  pos ??= bind.getLocalFlutterOption(k: windowFramePrefix + type.name);

  var lpos = LastWindowPosition.loadFromString(pos);
  if (lpos == null) {
    debugPrint("No window position saved, trying to center the window.");
    switch (type) {
      case WindowType.Main:
        // Center the main window only if no position is saved (on first run).
        if (isWindows || isLinux) {
          await windowManager.center();
        }
        // For MacOS, the window is already centered by default.
        // See https://github.com/rustdesk/rustdesk/blob/9b9276e7524523d7f667fefcd0694d981443df0e/flutter/macos/Runner/Base.lproj/MainMenu.xib#L333
        // If `<windowPositionMask>` in `<window>` is not set, the window will be centered.
        break;
      default:
        // No need to change the position of a sub window if no position is saved,
        // since the default position is already centered.
        // https://github.com/rustdesk/rustdesk/blob/317639169359936f7f9f85ef445ec9774218772d/flutter/lib/utils/multi_window_manager.dart#L163
        break;
    }
    return true;
  }
  if (type == WindowType.RemoteDesktop || type == WindowType.ViewCamera) {
    if (!isRemotePeerPos && windowId != null) {
      if (lpos.offsetWidth != null) {
        lpos.offsetWidth = lpos.offsetWidth! + windowId * kNewWindowOffset;
      }
      if (lpos.offsetHeight != null) {
        lpos.offsetHeight = lpos.offsetHeight! + windowId * kNewWindowOffset;
      }
    }
    if (display != null) {
      if (lpos.offsetWidth != null) {
        lpos.offsetWidth = lpos.offsetWidth! + display * kNewWindowOffset;
      }
      if (lpos.offsetHeight != null) {
        lpos.offsetHeight = lpos.offsetHeight! + display * kNewWindowOffset;
      }
    }
  }

  final size = await _adjustRestoreMainWindowSize(lpos.width, lpos.height);
  final offsetLeftTop = await _adjustRestoreMainWindowOffset(
    lpos.offsetWidth,
    lpos.offsetHeight,
    size.width,
    size.height,
  );
  debugPrint(
      "restore lpos: ${size.width}/${size.height}, offset:${offsetLeftTop?.dx}/${offsetLeftTop?.dy}, isMaximized: ${lpos.isMaximized}, isFullscreen: ${lpos.isFullscreen}");

  switch (type) {
    case WindowType.Main:
      restorePos() async {
        if (offsetLeftTop == null) {
          await windowManager.center();
        } else {
          await windowManager.setPosition(offsetLeftTop,
              ignoreDevicePixelRatio: _ignoreDevicePixelRatio);
        }
      }
      if (lpos.isMaximized == true) {
        await restorePos();
        if (!(bind.isIncomingOnly() || bind.isOutgoingOnly())) {
          await windowManager.maximize();
        }
      } else {
        final storeSize = !bind.isIncomingOnly() || bind.isOutgoingOnly();
        if (isWindows) {
          if (storeSize) {
            // We need to set the window size first to avoid the incorrect size in some special cases.
            // E.g. There are two monitors, the left one is 100% DPI and the right one is 175% DPI.
            // The window belongs to the left monitor, but if it is moved a little to the right, it will belong to the right monitor.
            // After restoring, the size will be incorrect.
            // See known issue in https://github.com/rustdesk/rustdesk/pull/9840
            await windowManager.setSize(size,
                ignoreDevicePixelRatio: _ignoreDevicePixelRatio);
          }
          await restorePos();
          if (storeSize) {
            await windowManager.setSize(size,
                ignoreDevicePixelRatio: _ignoreDevicePixelRatio);
          }
        } else {
          if (storeSize) {
            await windowManager.setSize(size,
                ignoreDevicePixelRatio: _ignoreDevicePixelRatio);
          }
          await restorePos();
        }
      }
      return true;
    default:
      final wc = WindowController.fromWindowId(windowId!);
      restoreFrame() async {
        if (offsetLeftTop == null) {
          await wc.center();
        } else {
          final frame = Rect.fromLTWH(
              offsetLeftTop.dx, offsetLeftTop.dy, size.width, size.height);
          await wc.setFrame(frame);
        }
      }
      if (lpos.isFullscreen == true) {
        if (!isMacOS) {
          await restoreFrame();
        }
        // An duration is needed to avoid the window being restored after fullscreen.
        Future.delayed(Duration(milliseconds: 300), () async {
          if (kWindowId == windowId) {
            stateGlobal.setFullscreen(true);
          } else {
            // If is not current window, we need to send a fullscreen message to `windowId`
            DesktopMultiWindow.invokeMethod(
                windowId, kWindowEventSetFullscreen, 'true');
          }
        });
      } else if (lpos.isMaximized == true) {
        await restoreFrame();
        // An duration is needed to avoid the window being restored after maximized.
        Future.delayed(Duration(milliseconds: 300), () async {
          await wc.maximize();
        });
      } else {
        await restoreFrame();
      }
      break;
  }
  return false;
}

var webInitialLink = "";

/// Initialize uni links for macos/windows
///
/// [Availability]
/// initUniLinks should only be used on macos/windows.
/// we use dbus for linux currently.
Future<bool> initUniLinks() async {
  if (isLinux) {
    return false;
  }
  // check cold boot
  try {
    final initialLink = await getInitialLink();
    print("initialLink: $initialLink");
    if (initialLink == null || initialLink.isEmpty) {
      return false;
    }
    if (isWeb) {
      webInitialLink = initialLink;
      return false;
    } else {
      return handleUriLink(uriString: initialLink);
    }
  } catch (err) {
    debugPrintStack(label: "$err");
    return false;
  }
}

/// Listen for uni links.
///
/// * handleByFlutter: Should uni links be handled by Flutter.
///
/// Returns a [StreamSubscription] which can listen the uni links.
StreamSubscription? listenUniLinks({handleByFlutter = true}) {
  if (isLinux || isWeb) {
    return null;
  }

  final sub = uriLinkStream.listen((Uri? uri) {
    debugPrint("A uri was received: $uri. handleByFlutter $handleByFlutter");
    if (uri != null) {
      if (handleByFlutter) {
        handleUriLink(uri: uri);
      } else {
        bind.sendUrlScheme(url: uri.toString());
      }
    } else {
      print("uni listen error: uri is empty.");
    }
  }, onError: (err) {
    print("uni links error: $err");
  });
  return sub;
}

enum UriLinkType {
  remoteDesktop,
  fileTransfer,
  viewCamera,
  portForward,
  rdp,
  terminal,
}

setEnvTerminalAdmin() {
  bind.mainSetEnv(key: 'IS_TERMINAL_ADMIN', value: 'Y');
}

// uri link handler
bool handleUriLink({List<String>? cmdArgs, Uri? uri, String? uriString}) {
  List<String>? args;
  if (cmdArgs != null && cmdArgs.isNotEmpty) {
    args = cmdArgs;
    // rustdesk <uri link>
    if (args[0].startsWith(bind.mainUriPrefixSync())) {
      final uri = Uri.tryParse(args[0]);
      if (uri != null) {
        args = urlLinkToCmdArgs(uri);
      }
    }
  } else if (uri != null) {
    args = urlLinkToCmdArgs(uri);
  } else if (uriString != null) {
    final uri = Uri.tryParse(uriString);
    if (uri != null) {
      args = urlLinkToCmdArgs(uri);
    }
  }
  if (args == null) {
    return false;
  }

  if (args.isEmpty) {
    windowOnTop(null);
    return true;
  }

  UriLinkType? type;
  String? id;
  String? password;
  String? switchUuid;
  bool? forceRelay;
  for (int i = 0; i < args.length; i++) {
    switch (args[i]) {
      case '--connect':
      case '--play':
        type = UriLinkType.remoteDesktop;
        id = args[i + 1];
        i++;
        break;
      case '--file-transfer':
        type = UriLinkType.fileTransfer;
        id = args[i + 1];
        i++;
        break;
      case '--view-camera':
        type = UriLinkType.viewCamera;
        id = args[i + 1];
        i++;
        break;
      case '--port-forward':
        type = UriLinkType.portForward;
        id = args[i + 1];
        i++;
        break;
      case '--rdp':
        type = UriLinkType.rdp;
        id = args[i + 1];
        i++;
        break;
      case '--terminal':
        type = UriLinkType.terminal;
        id = args[i + 1];
        i++;
        break;
      case '--terminal-admin':
        setEnvTerminalAdmin();
        type = UriLinkType.terminal;
        id = args[i + 1];
        i++;
        break;
      case '--password':
        password = args[i + 1];
        i++;
        break;
      case '--switch_uuid':
        switchUuid = args[i + 1];
        i++;
        break;
      case '--relay':
        forceRelay = true;
        break;
      default:
        break;
    }
  }
  if (type != null && id != null) {
    switch (type) {
      case UriLinkType.remoteDesktop:
        Future.delayed(Duration.zero, () {
          rustDeskWinManager.newRemoteDesktop(id!,
              password: password,
              switchUuid: switchUuid,
              forceRelay: forceRelay);
        });
        break;
      case UriLinkType.fileTransfer:
        Future.delayed(Duration.zero, () {
          rustDeskWinManager.newFileTransfer(id!,
              password: password, forceRelay: forceRelay);
        });
        break;
      case UriLinkType.viewCamera:
        Future.delayed(Duration.zero, () {
          rustDeskWinManager.newViewCamera(id!,
              password: password, forceRelay: forceRelay);
        });
        break;
      case UriLinkType.portForward:
        Future.delayed(Duration.zero, () {
          rustDeskWinManager.newPortForward(id!, false,
              password: password, forceRelay: forceRelay);
        });
        break;
      case UriLinkType.rdp:
        Future.delayed(Duration.zero, () {
          rustDeskWinManager.newPortForward(id!, true,
              password: password, forceRelay: forceRelay);
        });
        break;
      case UriLinkType.terminal:
        Future.delayed(Duration.zero, () {
          rustDeskWinManager.newTerminal(id!,
              password: password, forceRelay: forceRelay);
        });
        break;
    }

    return true;
  }

  return false;
}

List<String>? urlLinkToCmdArgs(Uri uri) {
  String? command;
  String? id;
  final options = [
    "connect",
    "play",
    "file-transfer",
    "view-camera",
    "port-forward",
    "rdp",
    "terminal",
    "terminal-admin",
  ];
  if (uri.authority.isEmpty &&
      uri.path.split('').every((char) => char == '/')) {
    return [];
  } else if (uri.authority == "connection" && uri.path.startsWith("/new/")) {
    // For compatibility
    command = '--connect';
    id = uri.path.substring("/new/".length);
  } else if (uri.authority == "config") {
    if (isAndroid || isIOS) {
      final allowDeepLinkServerSettings =
          bind.mainGetBuildinOption(key: kOptionAllowDeepLinkServerSettings) ==
              'Y';
      if (!allowDeepLinkServerSettings) {
        debugPrint(
            "Ignore rustdesk://config because $kOptionAllowDeepLinkServerSettings is not enabled.");
        // Keep the user-facing error generic; detailed rejection reason is in debug logs.
        // Delay toast to avoid missing overlay during cold-start deeplink handling.
        Timer(Duration(seconds: 1), () {
          showToast(translate('Failed'));
        });
        return null;
      }
      final config = uri.path.substring("/".length);
      // add a timer to make showToast work
      Timer(Duration(seconds: 1), () {
        importConfig(null, null, config);
      });
    }
    return null;
  } else if (uri.authority == "password") {
    if (isAndroid || isIOS) {
      final allowDeepLinkPassword =
          bind.mainGetBuildinOption(key: kOptionAllowDeepLinkPassword) == 'Y';
      if (!allowDeepLinkPassword) {
        debugPrint(
            "Ignore rustdesk://password because $kOptionAllowDeepLinkPassword is not enabled.");
        // Keep the user-facing error generic; detailed rejection reason is in debug logs.
        // Delay toast to avoid missing overlay during cold-start deeplink handling.
        Timer(Duration(seconds: 1), () {
          showToast(translate('Failed'));
        });
        return null;
      }
      final password = uri.path.substring("/".length);
      if (password.isNotEmpty) {
        Timer(Duration(seconds: 1), () async {
          final ok =
              await bind.mainSetPermanentPasswordWithResult(password: password);
          showToast(translate(ok ? 'Successful' : 'Failed'));
        });
      }
    }
  } else if (options.contains(uri.authority)) {
    command = '--${uri.authority}';
    if (uri.path.length > 1) {
      id = uri.path.substring(1);
    }
  } else if (uri.authority.length > 2 &&
      (uri.path.length <= 1 ||
          (uri.path == '/r' || uri.path.startsWith('/r@')))) {
    // rustdesk://<connect-id>
    // rustdesk://<connect-id>/r
    // rustdesk://<connect-id>/r@<server>
    command = '--connect';
    id = uri.authority;
    if (uri.path.length > 1) {
      id = id + uri.path;
    }
  }

  var queryParameters =
      uri.queryParameters.map((k, v) => MapEntry(k.toLowerCase(), v));

  var key = queryParameters["key"];
  if (id != null) {
    if (key != null) {
      id = "$id?key=$key";
    }
  }

  if (isMobile && id != null) {
    final forceRelay = queryParameters["relay"] != null;
    final password = queryParameters["password"];

    // Determine connection type based on command
    if (command == '--file-transfer') {
      connect(Get.context!, id,
          isFileTransfer: true, forceRelay: forceRelay, password: password);
    } else if (command == '--view-camera') {
      connect(Get.context!, id,
          isViewCamera: true, forceRelay: forceRelay, password: password);
    } else if (command == '--terminal') {
      connect(Get.context!, id,
          isTerminal: true, forceRelay: forceRelay, password: password);
    } else if (command == 'terminal-admin') {
      setEnvTerminalAdmin();
      connect(Get.context!, id,
          isTerminal: true, forceRelay: forceRelay, password: password);
    } else {
      // Default to remote desktop for '--connect', '--play', or direct connection
      connect(Get.context!, id, forceRelay: forceRelay, password: password);
    }
    return null;
  }

  List<String> args = List.empty(growable: true);
  if (command != null && id != null) {
    args.add(command);
    args.add(id);
    var param = queryParameters;
    String? password = param["password"];
    if (password != null) args.addAll(['--password', password]);
    String? switch_uuid = param["switch_uuid"];
    if (switch_uuid != null) args.addAll(['--switch_uuid', switch_uuid]);
    if (param["relay"] != null) args.add("--relay");
    return args;
  }

  return null;
}

connectMainDesktop(String id,
    {required bool isFileTransfer,
    required bool isViewCamera,
    required bool isTerminal,
    required bool isTcpTunneling,
    required bool isRDP,
    bool? forceRelay,
    String? password,
    String? connToken,
    bool? isSharedPassword}) async {
  if (isFileTransfer) {
    await rustDeskWinManager.newFileTransfer(id,
        password: password,
        isSharedPassword: isSharedPassword,
        connToken: connToken,
        forceRelay: forceRelay);
  } else if (isViewCamera) {
    await rustDeskWinManager.newViewCamera(id,
        password: password,
        isSharedPassword: isSharedPassword,
        connToken: connToken,
        forceRelay: forceRelay);
  } else if (isTcpTunneling || isRDP) {
    await rustDeskWinManager.newPortForward(id, isRDP,
        password: password,
        isSharedPassword: isSharedPassword,
        connToken: connToken,
        forceRelay: forceRelay);
  } else if (isTerminal) {
    await rustDeskWinManager.newTerminal(id,
        password: password,
        isSharedPassword: isSharedPassword,
        connToken: connToken,
        forceRelay: forceRelay);
  } else {
    await rustDeskWinManager.newRemoteDesktop(id,
        password: password,
        isSharedPassword: isSharedPassword,
        forceRelay: forceRelay);
  }
}

/// Connect to a peer with [id].
/// If [isFileTransfer], starts a session only for file transfer.
/// If [isViewCamera], starts a session only for view camera.
/// If [isTcpTunneling], starts a session only for tcp tunneling.
/// If [isRDP], starts a session only for rdp.
connect(BuildContext context, String id,
    {bool isFileTransfer = false,
    bool isViewCamera = false,
    bool isTerminal = false,
    bool isTcpTunneling = false,
    bool isRDP = false,
    bool forceRelay = false,
    String? password,
    String? connToken,
    bool? isSharedPassword}) async {
  if (id == '') return;
  if (!isDesktop || desktopType == DesktopType.main) {
    try {
      if (Get.isRegistered<IDTextEditingController>()) {
        final idController = Get.find<IDTextEditingController>();
        idController.text = formatID(id);
      }
      if (Get.isRegistered<TextEditingController>()) {
        final fieldTextEditingController = Get.find<TextEditingController>();
        fieldTextEditingController.text = formatID(id);
      }
    } catch (_) {}
  }
  id = id.replaceAll(' ', '');
  final oldId = id;
  id = await bind.mainHandleRelayId(id: id);
  forceRelay = id != oldId || forceRelay;
  assert(!(isFileTransfer && isTcpTunneling && isRDP),
      "more than one connect type");

  if (isDesktop) {
    if (desktopType == DesktopType.main) {
      await connectMainDesktop(
        id,
        isFileTransfer: isFileTransfer,
        isViewCamera: isViewCamera,
        isTerminal: isTerminal,
        isTcpTunneling: isTcpTunneling,
        isRDP: isRDP,
        password: password,
        isSharedPassword: isSharedPassword,
        forceRelay: forceRelay,
      );
    } else {
      await rustDeskWinManager.call(WindowType.Main, kWindowConnect, {
        'id': id,
        'isFileTransfer': isFileTransfer,
        'isViewCamera': isViewCamera,
        'isTerminal': isTerminal,
        'isTcpTunneling': isTcpTunneling,
        'isRDP': isRDP,
        'password': password,
        'isSharedPassword': isSharedPassword,
        'forceRelay': forceRelay,
        'connToken': connToken,
      });
    }
  } else {
    if (isFileTransfer) {
      if (isWeb) {
        Navigator.push(
          context,
          MaterialPageRoute(
            builder: (BuildContext context) =>
                desktop_file_manager.FileManagerPage(
                    id: id,
                    password: password,
                    isSharedPassword: isSharedPassword),
          ),
        );
      } else {
        Navigator.push(
          context,
          MaterialPageRoute(
            builder: (BuildContext context) => FileManagerPage(
                id: id,
                password: password,
                isSharedPassword: isSharedPassword,
                forceRelay: forceRelay),
          ),
        );
      }
    } else if (isViewCamera) {
      if (isWeb) {
        Navigator.push(
          context,
          MaterialPageRoute(
            builder: (BuildContext context) =>
                desktop_view_camera.ViewCameraPage(
              key: ValueKey(id),
              id: id,
              toolbarState: ToolbarState(),
              password: password,
              isSharedPassword: isSharedPassword,
            ),
          ),
        );
      } else {
        Navigator.push(
          context,
          MaterialPageRoute(
            builder: (BuildContext context) => ViewCameraPage(
                id: id,
                password: password,
                isSharedPassword: isSharedPassword,
                forceRelay: forceRelay),
          ),
        );
      }
    } else if (isTerminal) {
      Navigator.push(
        context,
        MaterialPageRoute(
          builder: (BuildContext context) => TerminalPage(
            id: id,
            password: password,
            isSharedPassword: isSharedPassword,
            forceRelay: forceRelay,
          ),
        ),
      );
    } else {
      if (isWeb) {
        Navigator.push(
          context,
          MaterialPageRoute(
            builder: (BuildContext context) => desktop_remote.RemotePage(
              key: ValueKey(id),
              id: id,
              toolbarState: ToolbarState(),
              password: password,
              isSharedPassword: isSharedPassword,
            ),
          ),
        );
      } else {
        Navigator.push(
          context,
          MaterialPageRoute(
            builder: (BuildContext context) => RemotePage(
                id: id,
                password: password,
                isSharedPassword: isSharedPassword,
                forceRelay: forceRelay),
          ),
        );
      }
    }
    stateGlobal.isInMainPage = false;
  }

  FocusScopeNode currentFocus = FocusScope.of(context);
  if (!currentFocus.hasPrimaryFocus) {
    currentFocus.unfocus();
  }
}

Map<String, String> getHttpHeaders() {
  return {
    'Authorization': 'Bearer ${bind.mainGetLocalOption(key: 'access_token')}'
  };
}

// Simple wrapper of built-in types for reference use.
class SimpleWrapper<T> {
  T value;
  SimpleWrapper(this.value);
}

/// Wakelock manager with reference counting for desktop.
/// Ensures wakelock is only disabled when all sessions are closed/minimized.
///
/// Note: Each isolate has its own WakelockPlus instance with independent assertion.
/// As long as one isolate has wakelock enabled, the screen stays awake.
/// This manager handles multiple tabs within the same isolate.
class WakelockManager {
  static final Set<UniqueKey> _enabledKeys = {};
  // Don't use WakelockPlus.enabled, it causes error on Android:
  // Unhandled Exception: FormatException: Message corrupted
  //
  // On Linux, multiple enable() calls create only one inhibit, but each disable()
  // only releases if _cookie != null. So we need our own _enabled state to avoid
  // calling disable() when not enabled.
  // See: https://github.com/fluttercommunity/wakelock_plus/blob/0c74e5bbc6aefac57b6c96bb7ef987705ed559ec/wakelock_plus/lib/src/wakelock_plus_linux_plugin.dart#L48
  static bool _enabled = false;

  static void enable(UniqueKey key, {bool isServer = false}) {
    // Check if we should keep awake during outgoing sessions
    if (!isServer) {
      final keepAwake =
          mainGetLocalBoolOptionSync(kOptionKeepAwakeDuringOutgoingSessions);
      if (!keepAwake) {
        return; // Don't enable wakelock if user disabled keep awake
      }
    }
    if (isDesktop) {
      _enabledKeys.add(key);
    }
    if (!_enabled) {
      _enabled = true;
      WakelockPlus.enable();
    }
  }

  static void disable(UniqueKey key) {
    if (isDesktop) {
      _enabledKeys.remove(key);
      if (_enabledKeys.isNotEmpty) {
        return;
      }
    }
    if (_enabled) {
      WakelockPlus.disable();
      _enabled = false;
    }
  }
}

/// call this to reload current window.
///
/// [Note]
/// Must have [RefreshWrapper] on the top of widget tree.
void reloadCurrentWindow() {
  if (Get.context != null) {
    // reload self window
    RefreshWrapper.of(Get.context!)?.rebuild();
  } else {
    debugPrint(
        "reload current window failed, global BuildContext does not exist");
  }
}

/// call this to reload all windows, including main + all sub windows.
Future<void> reloadAllWindows() async {
  reloadCurrentWindow();
  try {
    final ids = await DesktopMultiWindow.getAllSubWindowIds();
    for (final id in ids) {
      DesktopMultiWindow.invokeMethod(id, kWindowActionRebuild);
    }
  } on AssertionError {
    // ignore
  }
}

/// Indicate the flutter app is running in portable mode.
///
/// [Note]
/// Portable build is only available on Windows.
bool isRunningInPortableMode() {
  if (!isWindows) {
    return false;
  }
  return bool.hasEnvironment(kEnvPortableExecutable);
}

/// Window status callback
Future<void> onActiveWindowChanged() async {
  print(
      "[MultiWindowHandler] active window changed: ${rustDeskWinManager.getActiveWindows()}");
  if (rustDeskWinManager.getActiveWindows().isEmpty) {
    // close all sub windows
    try {
      if (isLinux) {
        await Future.wait([
          saveWindowPosition(WindowType.Main),
          rustDeskWinManager.closeAllSubWindows()
        ]);
      } else {
        await rustDeskWinManager.closeAllSubWindows();
      }
    } catch (err) {
      debugPrintStack(label: "$err");
    } finally {
      debugPrint("Start closing RustDesk...");
      await windowManager.setPreventClose(false);
      await windowManager.close();
      if (isMacOS) {
        // If we call without delay, `flutter/macos/Runner/MainFlutterWindow.swift` can handle the "terminate" event.
        // But the app will not close.
        //
        // No idea why we need to delay here, `terminate()` itself is also an async function.
        //
        // A quick workaround, use `Timer.periodic` to avoid the app not closing.
        // Because `await windowManager.close()` and `RdPlatformChannel.instance.terminate()`
        // may not work since `Flutter 3.24.4`, see the following logs.
        // A delay will allow the app to close.
        //
        //```
        // embedder.cc (2725): 'FlutterPlatformMessageCreateResponseHandle' returned 'kInvalidArguments'. Engine handle was invalid.
        // 2024-11-11 11:41:11.546 RustDesk[90272:2567686] Failed to create a FlutterPlatformMessageResponseHandle (2)
        // embedder.cc (2672): 'FlutterEngineSendPlatformMessage' returned 'kInvalidArguments'. Invalid engine handle.
        // 2024-11-11 11:41:11.565 RustDesk[90272:2567686] Failed to send message to Flutter engine on channel 'flutter/lifecycle' (2).
        // ```
        periodic_immediate(
            Duration(milliseconds: 30), RdPlatformChannel.instance.terminate);
      }
    }
  }
}

Timer periodic_immediate(Duration duration, Future<void> Function() callback) {
  Future.delayed(Duration.zero, callback);
  return Timer.periodic(duration, (timer) async {
    await callback();
  });
}

/// return a human readable windows version
WindowsTarget getWindowsTarget(int buildNumber) {
  if (!isWindows) {
    return WindowsTarget.naw;
  }
  if (buildNumber >= 22000) {
    return WindowsTarget.w11;
  } else if (buildNumber >= 10240) {
    return WindowsTarget.w10;
  } else if (buildNumber >= 9600) {
    return WindowsTarget.w8_1;
  } else if (buildNumber >= 9200) {
    return WindowsTarget.w8;
  } else if (buildNumber >= 7601) {
    return WindowsTarget.w7;
  } else if (buildNumber >= 6002) {
    return WindowsTarget.vista;
  } else {
    // minimum support
    return WindowsTarget.xp;
  }
}

/// Get windows target build number.
///
/// [Note]
/// Please use this function wrapped with `Platform.isWindows`.
int getWindowsTargetBuildNumber() {
  return getWindowsTargetBuildNumber_();
}

/// Indicating we need to use compatible ui mode.
///
/// [Conditions]
/// - Windows 7, window will overflow when we use frameless ui.
bool get kUseCompatibleUiMode =>
    isWindows &&
    const [WindowsTarget.w7].contains(windowsBuildNumber.windowsVersion);

bool get isWin10 => windowsBuildNumber.windowsVersion == WindowsTarget.w10;

class ServerConfig {
  late String idServer;
  late String relayServer;
  late String apiServer;
  late String key;

  ServerConfig(
      {String? idServer, String? relayServer, String? apiServer, String? key}) {
    this.idServer = idServer?.trim() ?? '';
    this.relayServer = relayServer?.trim() ?? '';
    this.apiServer = apiServer?.trim() ?? '';
    this.key = key?.trim() ?? '';
  }

  /// decode from shared string (from user shared or rustdesk-server generated)
  /// also see [encode]
  /// throw when decoding failure
  ServerConfig.decode(String msg) {
    var json = {};
    try {
      // back compatible
      json = jsonDecode(msg);
    } catch (err) {
      final input = msg.split('').reversed.join('');
      final bytes = base64Decode(base64.normalize(input));
      json = jsonDecode(utf8.decode(bytes, allowMalformed: true));
    }
    idServer = json['host'] ?? '';
    relayServer = json['relay'] ?? '';
    apiServer = json['api'] ?? '';
    key = json['key'] ?? '';
  }

  /// encode to shared string
  /// also see [ServerConfig.decode]
  String encode() {
    Map<String, String> config = {};
    config['host'] = idServer.trim();
    config['relay'] = relayServer.trim();
    config['api'] = apiServer.trim();
    config['key'] = key.trim();
    return base64UrlEncode(Uint8List.fromList(jsonEncode(config).codeUnits))
        .split('')
        .reversed
        .join();
  }

  /// from local options
  ServerConfig.fromOptions(Map<String, dynamic> options)
      : idServer = options['custom-rendezvous-server'] ?? "",
        relayServer = options['relay-server'] ?? "",
        apiServer = options['api-server'] ?? "",
        key = options['key'] ?? "";
}

Widget dialogButton(String text,
    {required VoidCallback? onPressed,
    bool isOutline = false,
    Widget? icon,
    TextStyle? style,
    ButtonStyle? buttonStyle}) {
  if (isDesktop || isWebDesktop) {
    if (isOutline) {
      return icon == null
          ? OutlinedButton(
              onPressed: onPressed,
              child: Text(translate(text), style: style),
            )
          : OutlinedButton.icon(
              icon: icon,
              onPressed: onPressed,
              label: Text(translate(text), style: style),
            );
    } else {
      return icon == null
          ? ElevatedButton(
              style: ElevatedButton.styleFrom(elevation: 0).merge(buttonStyle),
              onPressed: onPressed,
              child: Text(translate(text), style: style),
            )
          : ElevatedButton.icon(
              icon: icon,
              style: ElevatedButton.styleFrom(elevation: 0).merge(buttonStyle),
              onPressed: onPressed,
              label: Text(translate(text), style: style),
            );
    }
  } else {
    return TextButton(
      onPressed: onPressed,
      child: Text(
        translate(text),
        style: style,
      ),
    );
  }
}

int versionCmp(String v1, String v2) {
  return bind.versionToNumber(v: v1) - bind.versionToNumber(v: v2);
}

String getWindowName({WindowType? overrideType}) {
  final name = bind.mainGetAppNameSync();
  switch (overrideType ?? kWindowType) {
    case WindowType.Main:
      return name;
    case WindowType.FileTransfer:
      return "File Transfer - $name";
    case WindowType.ViewCamera:
      return "View Camera - $name";
    case WindowType.PortForward:
      return "Port Forward - $name";
    case WindowType.RemoteDesktop:
      return "Remote Desktop - $name";
    default:
      break;
  }
  return name;
}

String getWindowNameWithId(String id, {WindowType? overrideType}) {
  return "${DesktopTab.tablabelGetter(id).value} - ${getWindowName(overrideType: overrideType)}";
}

Future<void> updateSystemWindowTheme() async {
  // Set system window theme for macOS.
  final userPreference = MyTheme.getThemeModePreference();
  if (userPreference != ThemeMode.system) {
    if (isMacOS) {
      await RdPlatformChannel.instance.changeSystemWindowTheme(
          userPreference == ThemeMode.light
              ? SystemWindowTheme.light
              : SystemWindowTheme.dark);
    }
  }
}

/// macOS only
///
/// Note: not found a general solution for rust based AVFoundation bingding.
/// [AVFoundation] crate has compile error.
const kMacOSPermChannel = MethodChannel("org.rustdesk.rustdesk/host");

enum PermissionAuthorizeType {
  undetermined,
  authorized,
  denied, // and restricted
}

Future<PermissionAuthorizeType> osxCanRecordAudio() async {
  int res = await kMacOSPermChannel.invokeMethod("canRecordAudio");
  print(res);
  if (res > 0) {
    return PermissionAuthorizeType.authorized;
  } else if (res == 0) {
    return PermissionAuthorizeType.undetermined;
  } else {
    return PermissionAuthorizeType.denied;
  }
}

Future<bool> osxRequestAudio() async {
  return await kMacOSPermChannel.invokeMethod("requestRecordAudio");
}

Widget futureBuilder(
    {required Future? future, required Widget Function(dynamic data) hasData}) {
  return FutureBuilder(
      future: future,
      builder: (BuildContext context, AsyncSnapshot snapshot) {
        if (snapshot.hasData) {
          return hasData(snapshot.data!);
        } else {
          if (snapshot.hasError) {
            debugPrint(snapshot.error.toString());
          }
          return Container();
        }
      });
}

void onCopyFingerprint(String value) {
  if (value.isNotEmpty) {
    Clipboard.setData(ClipboardData(text: value));
    showToast('$value\n${translate("Copied")}');
  } else {
    showToast(translate("no fingerprints"));
  }
}

void onCopyId(String value) {
  if (value.isNotEmpty) {
    Clipboard.setData(ClipboardData(text: value));
    showToast('$value\n${translate("Copied")}');
  } else {
    showToast(translate("Invalid ID"));
  }
}

Future<bool> callMainCheckSuperUserPermission() async {
  bool checked = await bind.mainCheckSuperUserPermission();
  if (isMacOS) {
    await windowManager.show();
  }
  return checked;
}

Future<void> start_service(bool is_start) async {
  bool checked = !bind.mainIsInstalled() ||
      !isMacOS ||
      await callMainCheckSuperUserPermission();
  if (checked) {
    mainSetBoolOption(kOptionStopService, !is_start);
  }
}

Future<bool> canBeBlocked() async {
  if (isWeb) {
    // Web can only act as a controller, never as a controlled side,
    // so it should never be blocked by a remote session.
    return false;
  }
  // First check control permission
  final controlPermission = await bind.mainGetCommon(
      key: "is-remote-modify-enabled-by-control-permissions");
  if (controlPermission == "true") {
    return false;
  } else if (controlPermission == "false") {
    return true;
  }

  // Check local settings
  var accessMode = await bind.mainGetOption(key: kOptionAccessMode);
  var isCustomAccessMode = accessMode != 'full' && accessMode != 'view';
  var option = option2bool(kOptionAllowRemoteConfigModification,
      await bind.mainGetOption(key: kOptionAllowRemoteConfigModification));
  return accessMode == 'view' || (isCustomAccessMode && !option);
}

// to-do: web not implemented
Future<void> shouldBeBlocked(RxBool block, WhetherUseRemoteBlock? use) async {
  if (use != null && !await use()) {
    block.value = false;
    return;
  }
  var time0 = DateTime.now().millisecondsSinceEpoch;
  await bind.mainCheckMouseTime();
  Timer(const Duration(milliseconds: 120), () async {
    var d = time0 - await bind.mainGetMouseTime();
    if (d < 120) {
      block.value = true;
    } else {
      block.value = false;
    }
  });
}

typedef WhetherUseRemoteBlock = Future<bool> Function();
Widget buildRemoteBlock(
    {required Widget child,
    required RxBool block,
    required bool mask,
    WhetherUseRemoteBlock? use}) {
  return Obx(() => MouseRegion(
        onEnter: (_) async {
          await shouldBeBlocked(block, use);
        },
        onExit: (event) => block.value = false,
        child: Stack(children: [
          // scope block tab
          preventMouseKeyBuilder(child: child, block: block.value),
          // mask block click, cm not block click and still use check_click_time to avoid block local click
          if (mask)
            Offstage(
                offstage: !block.value,
                child: Container(
                  color: Colors.black.withOpacity(0.5),
                )),
        ]),
      ));
}

Widget preventMouseKeyBuilder({required Widget child, required bool block}) {
  return ExcludeFocus(
      excluding: block, child: AbsorbPointer(child: child, absorbing: block));
}

Widget unreadMessageCountBuilder(RxInt? count,
    {double? size, double? fontSize}) {
  return Obx(() => Offstage(
      offstage: !((count?.value ?? 0) > 0),
      child: Container(
        width: size ?? 16,
        height: size ?? 16,
        decoration: BoxDecoration(
          color: Colors.red,
          shape: BoxShape.circle,
        ),
        child: Center(
          child: Text("${count?.value ?? 0}",
              maxLines: 1,
              style: TextStyle(color: Colors.white, fontSize: fontSize ?? 10)),
        ),
      )));
}

Widget unreadTopRightBuilder(RxInt? count, {Widget? icon}) {
  return Stack(
    children: [
      icon ?? Icon(Icons.chat),
      Positioned(
          top: 0,
          right: 0,
          child: unreadMessageCountBuilder(count, size: 12, fontSize: 8))
    ],
  );
}

String toCapitalized(String s) {
  if (s.isEmpty) {
    return s;
  }
  return s.substring(0, 1).toUpperCase() + s.substring(1);
}

Widget buildErrorBanner(BuildContext context,
    {required RxBool loading,
    required RxString err,
    required Function? retry,
    required Function close}) {
  return Obx(() => Offstage(
        offstage: !(!loading.value && err.value.isNotEmpty),
        child: Center(
            child: Container(
          color: MyTheme.color(context).errorBannerBg,
          child: Row(
            mainAxisAlignment: MainAxisAlignment.center,
            crossAxisAlignment: CrossAxisAlignment.center,
            children: [
              FittedBox(
                child: Icon(
                  Icons.info,
                  color: Color.fromARGB(255, 249, 81, 81),
                ),
              ).marginAll(4),
              Flexible(
                child: Align(
                    alignment: Alignment.centerLeft,
                    child: Tooltip(
                      message: translate(err.value),
                      child: SelectableText(
                        translate(err.value),
                      ),
                    )).marginSymmetric(vertical: 2),
              ),
              if (retry != null)
                InkWell(
                    onTap: () {
                      retry.call();
                    },
                    child: Text(
                      translate("Retry"),
                      style: TextStyle(color: MyTheme.accent),
                    )).marginSymmetric(horizontal: 5),
              FittedBox(
                child: InkWell(
                  onTap: () {
                    close.call();
                  },
                  child: Icon(Icons.close).marginSymmetric(horizontal: 5),
                ),
              ).marginAll(4)
            ],
          ),
        )).marginOnly(bottom: 14),
      ));
}

String getDesktopTabLabel(String peerId, String alias) {
  String label = alias.isEmpty ? peerId : alias;
  try {
    String peer = bind.mainGetPeerSync(id: peerId);
    Map<String, dynamic> config = jsonDecode(peer);
    if (config['info']['hostname'] is String) {
      String hostname = config['info']['hostname'];
      if (hostname.isNotEmpty &&
          !label.toLowerCase().contains(hostname.toLowerCase())) {
        label += "@$hostname";
      }
    }
  } catch (e) {
    debugPrint("Failed to get hostname:$e");
  }
  return label;
}

sessionRefreshVideo(SessionID sessionId, PeerInfo pi) async {
  if (pi.currentDisplay == kAllDisplayValue) {
    for (int i = 0; i < pi.displays.length; i++) {
      await bind.sessionRefresh(sessionId: sessionId, display: i);
    }
  } else {
    await bind.sessionRefresh(sessionId: sessionId, display: pi.currentDisplay);
  }
}

Future<List<Rect>> getScreenListWayland() async {
  final screenRectList = <Rect>[];
  if (isMainDesktopWindow) {
    for (var screen in await window_size.getScreenList()) {
      final scale = kIgnoreDpi ? 1.0 : screen.scaleFactor;
      double l = screen.frame.left;
      double t = screen.frame.top;
      double r = screen.frame.right;
      double b = screen.frame.bottom;
      final rect = Rect.fromLTRB(l / scale, t / scale, r / scale, b / scale);
      screenRectList.add(rect);
    }
  } else {
    final screenList = await rustDeskWinManager.call(
        WindowType.Main, kWindowGetScreenList, '');
    try {
      for (var screen in jsonDecode(screenList.result) as List<dynamic>) {
        final scale = kIgnoreDpi ? 1.0 : screen['scaleFactor'];
        double l = screen['frame']['l'];
        double t = screen['frame']['t'];
        double r = screen['frame']['r'];
        double b = screen['frame']['b'];
        final rect = Rect.fromLTRB(l / scale, t / scale, r / scale, b / scale);
        screenRectList.add(rect);
      }
    } catch (e) {
      debugPrint('Failed to parse screenList: $e');
    }
  }
  return screenRectList;
}

Future<List<Rect>> getScreenListNotWayland() async {
  final screenRectList = <Rect>[];
  final displays = bind.mainGetDisplays();
  if (displays.isEmpty) {
    return screenRectList;
  }
  try {
    for (var display in jsonDecode(displays) as List<dynamic>) {
      // to-do: scale factor ?
      // final scale = kIgnoreDpi ? 1.0 : screen.scaleFactor;
      double l = display['x'].toDouble();
      double t = display['y'].toDouble();
      double r = (display['x'] + display['w']).toDouble();
      double b = (display['y'] + display['h']).toDouble();
      screenRectList.add(Rect.fromLTRB(l, t, r, b));
    }
  } catch (e) {
    debugPrint('Failed to parse displays: $e');
  }
  return screenRectList;
}

Future<List<Rect>> getScreenRectList() async {
  return bind.mainCurrentIsWayland()
      ? await getScreenListWayland()
      : await getScreenListNotWayland();
}

openMonitorInTheSameTab(int i, FFI ffi, PeerInfo pi,
    {bool updateCursorPos = true, bool recordSelection = true}) {
  if (isDisplayPanel) return; // a panel shows one fixed display
  if (isAndroid && androidDisplayPanels.isOpen) {
    // A switch replaces the host's capture set: the panels go first.
    androidDisplayPanels.closeAll().then((_) => _openMonitorInTheSameTab(
        i, ffi, pi,
        updateCursorPos: updateCursorPos, recordSelection: recordSelection));
    return;
  }
  _openMonitorInTheSameTab(i, ffi, pi,
      updateCursorPos: updateCursorPos, recordSelection: recordSelection);
}

_openMonitorInTheSameTab(int i, FFI ffi, PeerInfo pi,
    {bool updateCursorPos = true, bool recordSelection = true}) {
  if (recordSelection) {
    if (i == kAllDisplayValue && pi.currentDisplay != kAllDisplayValue) {
      // Where the mobile toolbar's "all displays" button goes back to.
      ffi.ffiModel.lastSingleDisplay = pi.currentDisplay;
    }
    ffi.ffiModel.lastUserDisplay = i;
    ffi.ffiModel.cancelPendingRestoreTimer();
    ffi.ffiModel.pendingMonitorRestore = null;
  }
  final displays = i == kAllDisplayValue
      ? List.generate(pi.displays.length, (index) => index)
      : [i];
  // Try clear image model before switching from all displays
  // 1. The remote side has multiple displays.
  // 2. Do not use texture render.
  // 3. Connect to Display 1.
  // 4. Switch to multi-displays `kAllDisplayValue`
  // 5. Switch to Display 2.
  // Then the remote page will display last picture of Display 1 at the beginning.
  if (pi.forceTextureRender && i != kAllDisplayValue) {
    ffi.imageModel.clearImage();
  }
  bind.sessionSwitchDisplay(
    isDesktop: isDesktop,
    sessionId: ffi.sessionId,
    value: Int32List.fromList(displays),
  );
  ffi.ffiModel.switchToNewDisplay(i, ffi.sessionId, ffi.id,
      updateCursorPos: updateCursorPos);
}

// Open new tab or window to show this monitor.
// For now just open new window.
//
// screenRect is used to move the new window to the specified screen and set fullscreen.
openMonitorInNewTabOrWindow(int i, String peerId, PeerInfo pi,
    {Rect? screenRect}) {
  final args = {
    'window_id': stateGlobal.windowId,
    'peer_id': peerId,
    'display': i,
    'display_count': pi.displays.length,
    'window_type': (kWindowType ?? WindowType.RemoteDesktop).index,
  };
  if (screenRect != null) {
    args['screen_rect'] = {
      'l': screenRect.left,
      't': screenRect.top,
      'r': screenRect.right,
      'b': screenRect.bottom,
    };
  }
  DesktopMultiWindow.invokeMethod(
      kMainWindowId, kWindowEventOpenMonitorSession, jsonEncode(args));
}

setNewConnectWindowFrame(int windowId, String peerId, int preSessionCount,
    WindowType windowType, int? display, Rect? screenRect) async {
  if (screenRect == null) {
    // Do not restore window position to new connection if there's a pre-session.
    // https://github.com/rustdesk/rustdesk/discussions/8825
    if (preSessionCount == 0) {
      await restoreWindowPosition(windowType,
          windowId: windowId, display: display, peerId: peerId);
    }
  } else {
    await tryMoveToScreenAndSetFullscreen(screenRect);
  }
}

tryMoveToScreenAndSetFullscreen(Rect? screenRect) async {
  if (screenRect == null) {
    return;
  }
  final wc = WindowController.fromWindowId(stateGlobal.windowId);
  final curFrame = await wc.getFrame();
  final frame =
      Rect.fromLTWH(screenRect.left + 30, screenRect.top + 30, 600, 400);
  if (stateGlobal.fullscreen.isTrue &&
      curFrame.left <= frame.left &&
      curFrame.top <= frame.top &&
      curFrame.width >= frame.width &&
      curFrame.height >= frame.height) {
    return;
  }
  await wc.setFrame(frame);
  // An duration is needed to avoid the window being restored after fullscreen.
  Future.delayed(Duration(milliseconds: 300), () async {
    stateGlobal.setFullscreen(true);
  });
}

parseParamScreenRect(Map<String, dynamic> params) {
  Rect? screenRect;
  if (params['screen_rect'] != null) {
    double l = params['screen_rect']['l'];
    double t = params['screen_rect']['t'];
    double r = params['screen_rect']['r'];
    double b = params['screen_rect']['b'];
    screenRect = Rect.fromLTRB(l, t, r, b);
  }
  return screenRect;
}

get isInputSourceFlutter => stateGlobal.getInputSource() == "Input source 2";

class _CountDownButton extends StatefulWidget {
  _CountDownButton({
    Key? key,
    required this.text,
    required this.second,
    required this.onPressed,
    this.submitOnTimeout = false,
  }) : super(key: key);
  final String text;
  final VoidCallback? onPressed;
  final int second;
  final bool submitOnTimeout;

  @override
  State<_CountDownButton> createState() => _CountDownButtonState();
}

class _CountDownButtonState extends State<_CountDownButton> {
  late int _countdownSeconds = widget.second;

  Timer? _timer;

  @override
  void initState() {
    super.initState();
    _startCountdownTimer();
  }

  @override
  void dispose() {
    _timer?.cancel();
    super.dispose();
  }

  void _startCountdownTimer() {
    _timer = Timer.periodic(Duration(seconds: 1), (timer) {
      if (_countdownSeconds <= 0) {
        timer.cancel();
        if (widget.submitOnTimeout) {
          widget.onPressed?.call();
        }
      } else {
        setState(() {
          _countdownSeconds--;
        });
      }
    });
  }

  @override
  Widget build(BuildContext context) {
    return dialogButton(
      '${translate(widget.text)} (${_countdownSeconds}s)',
      onPressed: widget.onPressed,
      isOutline: true,
    );
  }
}

importConfig(List<TextEditingController>? controllers, List<RxString>? errMsgs,
    String? text) {
  text = text?.trim();
  if (text != null && text.isNotEmpty) {
    try {
      final sc = ServerConfig.decode(text);
      if (isWeb || isIOS) {
        sc.relayServer = '';
      }
      if (sc.idServer.isNotEmpty) {
        Future<bool> success = setServerConfig(controllers, errMsgs, sc);
        success.then((value) {
          if (value) {
            showToast(translate('Import server configuration successfully'));
          } else {
            showToast(translate('Invalid server configuration'));
          }
        });
      } else {
        showToast(translate('Invalid server configuration'));
      }
      return sc;
    } catch (e) {
      showToast(translate('Invalid server configuration'));
    }
  } else {
    showToast(translate('Clipboard is empty'));
  }
}

Future<bool> setServerConfig(
  List<TextEditingController>? controllers,
  List<RxString>? errMsgs,
  ServerConfig config,
) async {
  String removeEndSlash(String input) {
    if (input.endsWith('/')) {
      return input.substring(0, input.length - 1);
    }
    return input;
  }

  config.idServer = removeEndSlash(config.idServer.trim());
  config.relayServer = removeEndSlash(config.relayServer.trim());
  config.apiServer = removeEndSlash(config.apiServer.trim());
  config.key = config.key.trim();
  if (controllers != null) {
    controllers[0].text = config.idServer;
    controllers[1].text = config.relayServer;
    controllers[2].text = config.apiServer;
    controllers[3].text = config.key;
  }
  // id
  if (config.idServer.isNotEmpty && errMsgs != null) {
    errMsgs[0].value = translate(await bind.mainTestIfValidServer(
        server: config.idServer, testWithProxy: true));
    if (errMsgs[0].isNotEmpty) {
      return false;
    }
  }
  // relay
  if (config.relayServer.isNotEmpty && errMsgs != null) {
    errMsgs[1].value = translate(await bind.mainTestIfValidServer(
        server: config.relayServer, testWithProxy: true));
    if (errMsgs[1].isNotEmpty) {
      return false;
    }
  }
  // api
  if (config.apiServer.isNotEmpty && errMsgs != null) {
    if (!config.apiServer.startsWith('http://') &&
        !config.apiServer.startsWith('https://')) {
      errMsgs[2].value =
          '${translate("API Server")}: ${translate("invalid_http")}';
      return false;
    }
  }
  final oldApiServer = await bind.mainGetApiServer();

  // should set one by one
  await bind.mainSetOption(
      key: 'custom-rendezvous-server', value: config.idServer);
  await bind.mainSetOption(key: 'relay-server', value: config.relayServer);
  await bind.mainSetOption(key: 'api-server', value: config.apiServer);
  await bind.mainSetOption(key: 'key', value: config.key);
  final newApiServer = await bind.mainGetApiServer();
  if (oldApiServer.isNotEmpty &&
      oldApiServer != newApiServer &&
      gFFI.userModel.isLogin) {
    gFFI.userModel.logOut(apiServer: oldApiServer);
  }
  return true;
}

ColorFilter? svgColor(Color? color) {
  if (color == null) {
    return null;
  } else {
    return ColorFilter.mode(color, BlendMode.srcIn);
  }
}

// ignore: must_be_immutable
class ComboBox extends StatelessWidget {
  late final List<String> keys;
  late final List<String> values;
  late final String initialKey;
  late final Function(String key) onChanged;
  late final bool enabled;
  late String current;

  ComboBox({
    Key? key,
    required this.keys,
    required this.values,
    required this.initialKey,
    required this.onChanged,
    this.enabled = true,
  }) : super(key: key);

  @override
  Widget build(BuildContext context) {
    var index = keys.indexOf(initialKey);
    if (index < 0) {
      index = 0;
    }
    var ref = values[index].obs;
    current = keys[index];
    return Container(
      decoration: BoxDecoration(
        border: Border.all(
          color: enabled
              ? MyTheme.color(context).border2 ?? MyTheme.border
              : MyTheme.border,
        ),
        borderRadius:
            BorderRadius.circular(8), //border raiuds of dropdown button
      ),
      height: 42, // should be the height of a TextField
      child: Obx(() => DropdownButton<String>(
            isExpanded: true,
            value: ref.value,
            elevation: 16,
            underline: Container(),
            style: TextStyle(
                color: enabled
                    ? Theme.of(context).textTheme.titleMedium?.color
                    : disabledTextColor(context, enabled)),
            icon: const Icon(
              Icons.expand_more_sharp,
              size: 20,
            ).marginOnly(right: 15),
            onChanged: enabled
                ? (String? newValue) {
                    if (newValue != null && newValue != ref.value) {
                      ref.value = newValue;
                      current = newValue;
                      onChanged(keys[values.indexOf(newValue)]);
                    }
                  }
                : null,
            items: values.map<DropdownMenuItem<String>>((String value) {
              return DropdownMenuItem<String>(
                value: value,
                child: Text(
                  value,
                  style: const TextStyle(fontSize: 15),
                  overflow: TextOverflow.ellipsis,
                ).marginOnly(left: 15),
              );
            }).toList(),
          )),
    ).marginOnly(bottom: 5);
  }
}

Color? disabledTextColor(BuildContext context, bool enabled) {
  return enabled
      ? null
      : Theme.of(context).textTheme.titleLarge?.color?.withOpacity(0.6);
}

Widget loadPowered(BuildContext context) {
  if (bind.mainGetBuildinOption(key: "hide-powered-by-me") == 'Y') {
    return SizedBox.shrink();
  }
  return MouseRegion(
    cursor: SystemMouseCursors.click,
    child: GestureDetector(
      onTap: () {
        launchUrl(Uri.parse('https://rustdesk.com'));
      },
      child: Opacity(
          opacity: 0.5,
          child: Text(
            translate("powered_by_me"),
            overflow: TextOverflow.clip,
            style: Theme.of(context)
                .textTheme
                .bodySmall
                ?.copyWith(fontSize: 9, decoration: TextDecoration.underline),
          )),
    ),
  ).marginOnly(top: 6);
}

const _kDefaultLogoAsset = 'assets/logo.png';
const _kLightLogoAsset = 'assets/logo_light.png';
const _kDarkLogoAsset = 'assets/logo_dark.png';

List<String> _logoAssetCandidatesForBrightness(Brightness brightness) {
  return brightness == Brightness.dark
      ? [_kDarkLogoAsset, _kDefaultLogoAsset]
      : [_kLightLogoAsset, _kDefaultLogoAsset];
}

Future<String?> _resolveLogoAsset(Brightness brightness) async {
  for (final asset in _logoAssetCandidatesForBrightness(brightness)) {
    try {
      await rootBundle.load(asset);
      return asset;
    } on FlutterError {
      continue;
    }
  }
  return null;
}

class _Logo extends StatefulWidget {
  const _Logo();

  @override
  State<_Logo> createState() => _LogoState();
}

class _LogoState extends State<_Logo> {
  final Map<Brightness, Future<String?>> _logoFutures = {};

  Future<String?> _logoFutureFor(Brightness brightness) {
    return _logoFutures.putIfAbsent(
      brightness,
      () => _resolveLogoAsset(brightness),
    );
  }

  @override
  Widget build(BuildContext context) {
    return FutureBuilder<String?>(
      future: _logoFutureFor(Theme.of(context).brightness),
      builder: (BuildContext context, AsyncSnapshot<String?> snapshot) {
        final asset = snapshot.data;
        if (asset != null) {
          final image = Image.asset(
            asset,
            fit: BoxFit.contain,
            errorBuilder: (ctx, error, stackTrace) {
              return Container();
            },
          );
          return Container(
            constraints: BoxConstraints(maxWidth: 300, maxHeight: 60),
            child: image,
          ).marginOnly(left: 12, right: 12, top: 12);
        }
        return const Offstage();
      },
    );
  }
}

// max 300 x 60
Widget loadLogo() => const _Logo();

Widget loadIcon(double size) {
  return Image.asset('assets/icon.png',
      width: size,
      height: size,
      errorBuilder: (ctx, error, stackTrace) => SvgPicture.asset(
            'assets/icon.svg',
            width: size,
            height: size,
          ));
}

var imcomingOnlyHomeSize = Size(280, 300);
Size getIncomingOnlyHomeSize() {
  final magicWidth = isWindows ? 11.0 : 2.0;
  final magicHeight = 10.0;
  return imcomingOnlyHomeSize +
      Offset(magicWidth, kDesktopRemoteTabBarHeight + magicHeight);
}

Size getIncomingOnlySettingsSize() {
  return Size(768, 600);
}

bool isInHomePage() {
  final controller = Get.find<DesktopTabController>();
  return controller.state.value.selected == 0;
}

Widget _buildPresetPasswordWarning() {
  if (bind.mainGetBuildinOption(key: kOptionRemovePresetPasswordWarning) !=
      'N') {
    return SizedBox.shrink();
  }
  return Container(
    color: Colors.yellow,
    child: Column(
      children: [
        Align(
            child: Text(
          translate("Security Alert"),
          style: TextStyle(
            color: Colors.red,
            fontSize:
                18, // https://github.com/rustdesk/rustdesk-server-pro/issues/261
            fontWeight: FontWeight.bold,
          ),
        )).paddingOnly(bottom: 8),
        Text(
          translate("preset_password_warning"),
          style: TextStyle(color: Colors.red),
        )
      ],
    ).paddingAll(8),
  ); // Show a warning message if the Future completed with true
}

Widget buildPresetPasswordWarningMobile() {
  if (bind.isPresetPasswordMobileOnly()) {
    return _buildPresetPasswordWarning();
  } else {
    return SizedBox.shrink();
  }
}

Widget buildPresetPasswordWarning() {
  return FutureBuilder<bool>(
    future: bind.isPresetPassword(),
    builder: (BuildContext context, AsyncSnapshot<bool> snapshot) {
      if (snapshot.connectionState == ConnectionState.waiting) {
        return CircularProgressIndicator(); // Show a loading spinner while waiting for the Future to complete
      } else if (snapshot.hasError) {
        return Text(
            'Error: ${snapshot.error}'); // Show an error message if the Future completed with an error
      } else if (snapshot.hasData && snapshot.data == true) {
        return _buildPresetPasswordWarning();
      } else {
        return SizedBox
            .shrink(); // Show nothing if the Future completed with false or null
      }
    },
  );
}

// https://github.com/leanflutter/window_manager/blob/87dd7a50b4cb47a375b9fc697f05e56eea0a2ab3/lib/src/widgets/virtual_window_frame.dart#L44
Widget buildVirtualWindowFrame(BuildContext context, Widget child) {
  boxShadow() => isMainDesktopWindow
      ? <BoxShadow>[
          if (stateGlobal.fullscreen.isFalse || stateGlobal.isMaximized.isFalse)
            BoxShadow(
              color: Colors.black.withOpacity(0.1),
              offset: Offset(
                  0.0,
                  stateGlobal.isFocused.isTrue
                      ? kFrameBoxShadowOffsetFocused
                      : kFrameBoxShadowOffsetUnfocused),
              blurRadius: kFrameBoxShadowBlurRadius,
            ),
        ]
      : null;
  return Obx(
    () => Container(
      decoration: BoxDecoration(
        color: isMainDesktopWindow
            ? Colors.transparent
            : Theme.of(context).colorScheme.background,
        border: Border.all(
          color: Theme.of(context).dividerColor,
          width: stateGlobal.windowBorderWidth.value,
        ),
        borderRadius: BorderRadius.circular(
          (stateGlobal.fullscreen.isTrue || stateGlobal.isMaximized.isTrue)
              ? 0
              : kFrameBorderRadius,
        ),
        boxShadow: boxShadow(),
      ),
      child: ClipRRect(
        borderRadius: BorderRadius.circular(
          (stateGlobal.fullscreen.isTrue || stateGlobal.isMaximized.isTrue)
              ? 0
              : kFrameClipRRectBorderRadius,
        ),
        child: child,
      ),
    ),
  );
}

get windowResizeEdgeSize =>
    isLinux && !_linuxWindowResizable ? 0.0 : kWindowResizeEdgeSize;

// `windowManager.setResizable(false)` will reset the window size to the default size on Linux and then set unresizable.
// See _linuxWindowResizable for more details.
// So we use `setResizable()` instead of `windowManager.setResizable()`.
//
// We can only call `windowManager.setResizable(false)` if we need the default size on Linux.
setResizable(bool resizable) {
  if (isLinux) {
    _linuxWindowResizable = resizable;
    stateGlobal.refreshResizeEdgeSize();
  } else {
    windowManager.setResizable(resizable);
  }
}

isOptionFixed(String key) => bind.mainIsOptionFixed(key: key);

bool isChangePermanentPasswordDisabled() =>
    bind.mainGetBuildinOption(key: kOptionDisableChangePermanentPassword) ==
    'Y';

bool isChangeIdDisabled() =>
    bind.mainGetBuildinOption(key: kOptionDisableChangeId) == 'Y';

bool isUnlockPinDisabled() =>
    bind.mainGetBuildinOption(key: kOptionDisableUnlockPin) == 'Y';

bool? _isCustomClient;
bool get isCustomClient {
  _isCustomClient ??= bind.isCustomClient();
  return _isCustomClient!;
}

get defaultOptionLang => isCustomClient ? 'default' : '';
get defaultOptionTheme => isCustomClient ? 'system' : '';
get defaultOptionYes => isCustomClient ? 'Y' : '';
get defaultOptionNo => isCustomClient ? 'N' : '';
get defaultOptionWhitelist => isCustomClient ? ',' : '';
get defaultOptionAccessMode => isCustomClient ? 'custom' : '';
get defaultOptionApproveMode => isCustomClient ? 'password-click' : '';

bool whitelistNotEmpty() {
  // https://rustdesk.com/docs/en/self-host/client-configuration/advanced-settings/#whitelist
  final v = bind.mainGetOptionSync(key: kOptionWhitelist);
  return v != '' && v != ',';
}

bool idWhitelistNotEmpty() {
  final v = bind.mainGetOptionSync(key: kOptionIdWhitelist);
  return v != '' && v != ',';
}

// `setMovable()` is only supported on macOS.
//
// On macOS, the window can be dragged by the tab bar by default.
// We need to disable the movable feature to prevent the window from being dragged by the tabs in the tab bar.
//
// When we drag the blank tab bar (not the tab), the window will be dragged normally by adding the `onPanStart` handle.
//
// See the following code for more details:
// https://github.com/rustdesk/rustdesk/blob/ce1dac3b8613596b4d8ae981275f9335489eb935/flutter/lib/desktop/widgets/tabbar_widget.dart#L385
// https://github.com/rustdesk/rustdesk/blob/ce1dac3b8613596b4d8ae981275f9335489eb935/flutter/lib/desktop/widgets/tabbar_widget.dart#L399
//
// @platforms macos
disableWindowMovable(int? windowId) {
  if (!isMacOS) {
    return;
  }

  if (windowId == null) {
    windowManager.setMovable(false);
  } else {
    WindowController.fromWindowId(windowId).setMovable(false);
  }
}

Widget netWorkErrorWidget() {
  return Center(
      child: Column(
    mainAxisAlignment: MainAxisAlignment.center,
    crossAxisAlignment: CrossAxisAlignment.center,
    children: [
      if (!gFFI.userModel.networkErrorFromServer.value)
        Text(translate("network_error_tip")),
      ElevatedButton(
              onPressed: gFFI.userModel.refreshCurrentUser,
              child: Text(translate("Retry")))
          .marginSymmetric(vertical: 16),
      SelectableText(gFFI.userModel.networkError.value,
          style: TextStyle(fontSize: 11, color: Colors.red)),
    ],
  ));
}

List<ResizeEdge>? get windowManagerEnableResizeEdges => isWindows
    ? [
        ResizeEdge.topLeft,
        ResizeEdge.top,
        ResizeEdge.topRight,
      ]
    : null;

List<SubWindowResizeEdge>? get subWindowManagerEnableResizeEdges => isWindows
    ? [
        SubWindowResizeEdge.topLeft,
        SubWindowResizeEdge.top,
        SubWindowResizeEdge.topRight,
      ]
    : null;

void earlyAssert() {
  assert('\1' == '1');
}

void checkUpdate() {
  if (!isWeb) {
    if (!bind.isCustomClient()) {
      platformFFI.registerEventHandler(
          kCheckSoftwareUpdateFinish, kCheckSoftwareUpdateFinish,
          (Map<String, dynamic> evt) async {
        if (evt['url'] is String) {
          stateGlobal.updateUrl.value = evt['url'];
        }
      });
      Timer(const Duration(seconds: 1), () async {
        bind.mainGetSoftwareUpdateUrl();
      });
    }
  }
}

// https://github.com/flutter/flutter/issues/153560#issuecomment-2497160535
// For TextField, TextFormField
extension WorkaroundFreezeLinuxMint on Widget {
  Widget workaroundFreezeLinuxMint() {
    // No need to check if is Linux Mint, because this workaround is harmless on other platforms.
    if (isLinux) {
      return ExcludeSemantics(child: this);
    } else {
      return this;
    }
  }
}

// Don't use `extension` here, the border looks weird if using `extension` in my test.
Widget workaroundWindowBorder(BuildContext context, Widget child) {
  if (!isWin10) {
    return child;
  }

  final isLight = Theme.of(context).brightness == Brightness.light;
  final borderColor = isLight ? Colors.black87 : Colors.grey;
  final width = isLight ? 0.5 : 0.1;

  getBorderWidget(Widget child) {
    return Obx(() =>
        (stateGlobal.isMaximized.isTrue || stateGlobal.fullscreen.isTrue)
            ? Offstage()
            : child);
  }

  final List<Widget> borders = [
    getBorderWidget(Container(
      color: borderColor,
      height: width + 0.1,
    ))
  ];
  if (kWindowType == WindowType.Main && !isLight) {
    borders.addAll([
      getBorderWidget(Align(
        alignment: Alignment.topLeft,
        child: Container(
          color: borderColor,
          width: width,
        ),
      )),
      getBorderWidget(Align(
        alignment: Alignment.topRight,
        child: Container(
          color: borderColor,
          width: width,
        ),
      )),
      getBorderWidget(Align(
        alignment: Alignment.bottomCenter,
        child: Container(
          color: borderColor,
          height: width,
        ),
      )),
    ]);
  }
  return Stack(
    children: [
      child,
      ...borders,
    ],
  );
}

void updateTextAndPreserveSelection(
    TextEditingController controller, String text) {
  // Only care about select all for now.
  final isSelected = controller.selection.isValid &&
      controller.selection.end > controller.selection.start;

  // Set text will make the selection invalid.
  controller.text = text;

  if (isSelected) {
    controller.selection = TextSelection(
        baseOffset: 0, extentOffset: controller.value.text.length);
  }
}

List<String> getPrinterNames() {
  final printerNamesJson = bind.mainGetPrinterNames();
  if (printerNamesJson.isEmpty) {
    return [];
  }
  try {
    final List<dynamic> printerNamesList = jsonDecode(printerNamesJson);
    final appPrinterName = '$appName Printer';
    return printerNamesList
        .map((e) => e.toString())
        .where((name) => name != appPrinterName)
        .toList();
  } catch (e) {
    debugPrint('failed to parse printer names, err: $e');
    return [];
  }
}

String _appName = '';
String get appName {
  if (_appName.isEmpty) {
    _appName = bind.mainGetAppNameSync();
  }
  return _appName;
}

String getConnectionText(bool secure, bool direct, String streamType) {
  String connectionText;
  if (secure && direct) {
    connectionText = translate("Direct and encrypted connection");
  } else if (secure && !direct) {
    connectionText = translate("Relayed and encrypted connection");
  } else if (!secure && direct) {
    connectionText = translate("Direct and unencrypted connection");
  } else {
    connectionText = translate("Relayed and unencrypted connection");
  }
  if (streamType == 'Relay') {
    streamType = 'TCP';
  }
  if (streamType.isEmpty) {
    return connectionText;
  } else {
    return '$connectionText ($streamType)';
  }
}

String decode_http_response(http.Response resp) {
  try {
    // https://github.com/rustdesk/rustdesk-server-pro/discussions/758
    return utf8.decode(resp.bodyBytes, allowMalformed: true);
  } catch (e) {
    debugPrint('Failed to decode response as UTF-8: $e');
    // Fallback to bodyString which handles encoding automatically
    return resp.body;
  }
}

bool peerTabShowNote(PeerTabIndex peerTabIndex) {
  return peerTabIndex == PeerTabIndex.ab || peerTabIndex == PeerTabIndex.group;
}

// TODO: We should support individual bits combinations in the future.
// But for now, just keep it simple, because the old code only supports single button.
// No users have requested multi-button support yet.
String mouseButtonsToPeer(int buttons) {
  switch (buttons) {
    case kPrimaryMouseButton:
      return 'left';
    case kSecondaryMouseButton:
      return 'right';
    case kMiddleMouseButton:
      return 'wheel';
    case kBackMouseButton:
      return 'back';
    case kForwardMouseButton:
      return 'forward';
    default:
      return '';
  }
}

/// Build an avatar widget from an avatar URL or data URI string.
/// Returns [fallback] if avatar is empty or cannot be decoded.
/// [borderRadius] defaults to [size]/2 (circle).
Widget? buildAvatarWidget({
  required String avatar,
  required double size,
  double? borderRadius,
  Widget? fallback,
}) {
  final trimmed = avatar.trim();
  if (trimmed.isEmpty) return fallback;

  ImageProvider? imageProvider;
  if (trimmed.startsWith('data:image/')) {
    final comma = trimmed.indexOf(',');
    if (comma > 0) {
      try {
        imageProvider = MemoryImage(base64Decode(trimmed.substring(comma + 1)));
      } catch (_) {}
    }
  } else if (trimmed.startsWith('http://') || trimmed.startsWith('https://')) {
    imageProvider = NetworkImage(trimmed);
  }

  if (imageProvider == null) return fallback;

  final radius = borderRadius ?? size / 2;
  return ClipRRect(
    borderRadius: BorderRadius.circular(radius),
    child: Image(
      image: imageProvider,
      width: size,
      height: size,
      fit: BoxFit.cover,
      errorBuilder: (_, __, ___) => fallback ?? SizedBox.shrink(),
    ),
  );
}
