# Preserve WikiActivity and all its methods including companion object statics
# These methods are called via JNI from native code

-keep class com.burningtreec.tiddlydesktop_rs.WikiActivity {
    *;
}

-keep class com.burningtreec.tiddlydesktop_rs.WikiActivity$Companion {
    *;
}

# Keep MainActivity
-keep class com.burningtreec.tiddlydesktop_rs.MainActivity {
    *;
}

# Keep WikiServerService for foreground notification (called via JNI)
-keep class com.burningtreec.tiddlydesktop_rs.WikiServerService {
    *;
}

-keep class com.burningtreec.tiddlydesktop_rs.WikiServerService$Companion {
    *;
}

# Keep widget providers
-keep class com.burningtreec.tiddlydesktop_rs.QuickCaptureWidgetProvider {
    *;
}

# Keep all @JavascriptInterface annotated methods
-keepclassmembers class * {
    @android.webkit.JavascriptInterface <methods>;
}
