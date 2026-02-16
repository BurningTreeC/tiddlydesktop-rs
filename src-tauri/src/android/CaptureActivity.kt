package com.burningtreec.tiddlydesktop_rs

import androidx.appcompat.app.AppCompatActivity
import android.app.AlertDialog
import android.content.Intent
import android.content.res.ColorStateList
import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.graphics.Color
import android.graphics.Outline
import android.graphics.drawable.GradientDrawable
import android.graphics.drawable.RippleDrawable
import android.net.Uri
import android.os.Bundle
import android.text.InputType
import android.util.Base64
import android.util.Log
import android.util.TypedValue
import android.view.Gravity
import android.view.View
import android.view.ViewGroup
import android.view.ViewOutlineProvider
import android.widget.*
import org.json.JSONArray
import org.json.JSONObject
import android.media.MediaMetadataRetriever
import android.webkit.MimeTypeMap
import androidx.documentfile.provider.DocumentFile
import com.google.android.material.button.MaterialButton
import java.io.ByteArrayOutputStream
import java.io.File
import java.net.HttpURLConnection
import java.net.URL
import java.util.regex.Pattern

/**
 * Quick Capture activity for sharing content from other apps into TiddlyWiki.
 *
 * Handles ACTION_SEND for text/plain and image intents.
 * Shows a dialog-themed UI where the user can edit title/tags, select target wiki,
 * and optionally clip a web page. Saves captures as JSON files in {filesDir}/captures/
 * which WikiActivity imports on next page load or resume.
 */
class CaptureActivity : AppCompatActivity() {

    companion object {
        private const val TAG = "CaptureActivity"
        private val URL_PATTERN: Pattern = Pattern.compile("""https?://\S+""")
        private val TRAIL_PUNCT = charArrayOf('.', ',', ';', '!', '?')

        /**
         * Trim trailing punctuation from a URL, but preserve balanced parentheses.
         * Wikipedia URLs like https://en.wikipedia.org/wiki/Android_(Betriebssystem)
         * have a meaningful closing ')' that must not be stripped.
         */
        fun trimUrlPunctuation(url: String): String {
            var end = url.length
            while (end > 0) {
                val ch = url[end - 1]
                if (ch in TRAIL_PUNCT) {
                    end--
                } else if (ch == ')') {
                    // Only strip closing paren if unbalanced (more ')' than '(')
                    val sub = url.substring(0, end)
                    if (sub.count { it == ')' } > sub.count { it == '(' }) {
                        end--
                    } else {
                        break
                    }
                } else {
                    break
                }
            }
            return if (end == url.length) url else url.substring(0, end)
        }

        /**
         * Sanitize a title string from external apps.
         * Strips zero-width chars, control chars, directional markers,
         * HTML entities, and normalizes whitespace.
         */
        fun sanitizeTitle(raw: String): String {
            var s = raw
            // Decode HTML entities
            s = s.replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">")
                .replace("&quot;", "\"").replace("&#39;", "'").replace("&apos;", "'")
                .replace("&nbsp;", " ")
                .replace(Regex("""&#(\d+);""")) { m ->
                    val code = m.groupValues[1].toIntOrNull()
                    if (code != null) String(Character.toChars(code)) else m.value
                }
                .replace(Regex("""&#x([0-9a-fA-F]+);""")) { m ->
                    val code = m.groupValues[1].toIntOrNull(16)
                    if (code != null) String(Character.toChars(code)) else m.value
                }
            // Strip zero-width and invisible Unicode characters:
            // U+200B (zero-width space), U+200C/D (zero-width non-joiner/joiner),
            // U+200E/F (LRM/RLM), U+202A-202E (bidi), U+2060 (word joiner),
            // U+FEFF (BOM/zero-width no-break space), U+00AD (soft hyphen)
            s = s.replace(Regex("[\u200B-\u200F\u202A-\u202E\u2060\uFEFF\u00AD]"), "")
            // Strip remaining control chars (C0/C1) except newline/tab
            s = s.replace(Regex("[\\x00-\\x08\\x0B\\x0C\\x0E-\\x1F\\x7F-\\x9F]"), "")
            // Normalize whitespace: collapse runs, trim
            s = s.replace(Regex("\\s+"), " ").trim()
            return s
        }


    }

    private var sharedText: String? = null
    private var sharedSubject: String? = null
    private var sharedImageUri: Uri? = null
    private var detectedUrl: String? = null
    private var imageBitmap: Bitmap? = null
    private var sharedFileUri: Uri? = null
    private var sharedTextFileUri: Uri? = null  // Original URI when text came from a shared .txt file
    private var fileThumbnail: Bitmap? = null
    private var importFileUri: Uri? = null
    private var importFileName: String? = null
    private var importFileMimeType: String? = null
    private var multipleUris: List<Uri>? = null
    private var multipleThumbnails: List<Bitmap?> = emptyList()
    private var captureType: String = "text/vnd.tiddlywiki"
    private var wikiList: List<WikiEntry> = emptyList()

    private var isBlankCapture: Boolean = false

    // UI references
    private var titleEdit: EditText? = null
    private var tagsEdit: EditText? = null
    private var contentEdit: EditText? = null
    private var wikiSpinner: Spinner? = null
    private var clipProgress: ProgressBar? = null
    private var clippedContent: String? = null

    data class WikiEntry(val path: String, val title: String, val isFolder: Boolean, val externalAttachments: Boolean = true)

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        resolveThemeColors()

        // Clean up stale captures on every launch
        cleanupStaleCaptureFiles()

        // Handle Quick Capture widget action (blank capture)
        if (intent.action == QuickCaptureWidgetProvider.ACTION_QUICK_CAPTURE) {
            wikiList = loadRecentWikis()
            if (wikiList.isEmpty()) {
                Toast.makeText(this, getString(R.string.capture_no_wikis), Toast.LENGTH_SHORT).show()
                finish()
                return
            }
            isBlankCapture = true
            buildUI()
            return
        }

        if (intent.action != Intent.ACTION_SEND && intent.action != Intent.ACTION_SEND_MULTIPLE) {
            Log.w(TAG, "Unsupported action: ${intent.action}")
            finish()
            return
        }

        wikiList = loadRecentWikis()

        when {
            intent.action == Intent.ACTION_SEND_MULTIPLE -> handleMultipleIntent()
            intent.type == "image/svg+xml" -> handleSvgIntent()
            intent.type == "text/plain" -> handleTextIntent()
            intent.type?.startsWith("image/") == true -> handleImageIntent()
            intent.type?.startsWith("video/") == true ||
            intent.type?.startsWith("audio/") == true ||
            intent.type == "application/pdf" -> handleFileIntent()
            intent.type == "text/markdown" ||
            intent.type == "text/x-markdown" -> handleMarkdownIntent()
            intent.type == "text/vcard" ||
            intent.type == "text/x-vcard" -> handleContactIntent()
            intent.type == "text/calendar" -> handleCalendarIntent()
            intent.type == "text/html" ||
            intent.type == "application/xhtml+xml" ||
            intent.type == "application/json" ||
            intent.type == "text/csv" ||
            intent.type == "text/css" ||
            intent.type == "application/javascript" ||
            intent.type == "text/javascript" -> handleNativeImportIntent()
            else -> {
                Log.w(TAG, "Unsupported type: ${intent.type}")
                Toast.makeText(this, getString(R.string.capture_unsupported_type), Toast.LENGTH_SHORT).show()
                finish()
                return
            }
        }

        buildUI()

        // Auto-fetch meta preview when a URL is detected
        if (detectedUrl != null) {
            startWebClip()
        }
    }

    private fun handleTextIntent() {
        // Check for shared file URI first (.txt file shared from file manager etc.)
        val fileUri = getStreamUri()
        if (fileUri != null) {
            val fileText = readTextFromUri(fileUri)
            if (!fileText.isNullOrBlank()) {
                sharedText = fileText
                sharedTextFileUri = fileUri
                captureType = "text/plain"
                sharedSubject = getDisplayName(fileUri)?.substringBeforeLast(".")
                    ?: intent.getStringExtra(Intent.EXTRA_SUBJECT)?.let { sanitizeTitle(it) }
                return
            }
        }
        // Fall back to text snippet (EXTRA_TEXT from browsers, share sheets, etc.)
        sharedText = intent.getStringExtra(Intent.EXTRA_TEXT)
        sharedSubject = intent.getStringExtra(Intent.EXTRA_SUBJECT)?.let { sanitizeTitle(it) }

        if (sharedText.isNullOrBlank()) {
            Toast.makeText(this, getString(R.string.capture_no_text), Toast.LENGTH_SHORT).show()
            finish()
            return
        }

        // Detect URL(s) in shared text.
        // Browsers append the page URL at the end of EXTRA_TEXT, so use the LAST URL.
        // Only remove that last URL from the text — other URLs are part of the content.
        val matcher = URL_PATTERN.matcher(sharedText!!)
        val allUrls = mutableListOf<Pair<String, Int>>() // raw url + start index
        while (matcher.find()) {
            allUrls.add(Pair(matcher.group(), matcher.start()))
        }

        // Also check EXTRA_SUBJECT for URLs (some apps put the URL there)
        val subjectUrl = sharedSubject?.let {
            val m = URL_PATTERN.matcher(it)
            if (m.find()) m.group() else null
        }

        if (allUrls.isNotEmpty()) {
            Log.d(TAG, "Found ${allUrls.size} URL(s) in EXTRA_TEXT: ${allUrls.map { it.first }}")
            // Strip trailing punctuation that isn't part of the URL
            val cleaned = allUrls.map { trimUrlPunctuation(it.first) }
            // Use the last URL (browser-appended page URL), but if a longer URL
            // shares the same domain, prefer it (handles root-vs-subpage).
            // Also include subjectUrl as a candidate.
            val lastUrl = cleaned.last()
            val lastDomain = try { URL(lastUrl).host } catch (_: Exception) { "" }
            val candidates = cleaned.toMutableList()
            if (subjectUrl != null) {
                val cleanedSubjectUrl = trimUrlPunctuation(subjectUrl)
                candidates.add(cleanedSubjectUrl)
            }
            val bestUrl = candidates.filter {
                try { URL(it).host == lastDomain } catch (_: Exception) { false }
            }.maxByOrNull { it.length } ?: lastUrl
            val rawUrl = bestUrl
            // Strip Chrome's text fragment (#:~:text=...) from URL for clean links
            detectedUrl = rawUrl.replace(Regex("#:~:.*$"), "")
            Log.d(TAG, "Selected URL: $detectedUrl")
            // Only remove the selected URL from sharedText — leave other URLs in place
            // as they're part of the user's selected content
            var remaining = sharedText!!.replace(rawUrl, "")
            // Also remove the original (pre-trim) version if different
            val origIdx = cleaned.indexOf(rawUrl)
            if (origIdx >= 0 && allUrls[origIdx].first != rawUrl) {
                remaining = remaining.replace(allUrls[origIdx].first, "")
            }
            sharedText = remaining.trim().ifBlank { null }
            // If the best URL came from EXTRA_SUBJECT, remove it from the subject too
            if (subjectUrl != null && detectedUrl == trimUrlPunctuation(subjectUrl).replace(Regex("#:~:.*$"), "")) {
                sharedSubject = sharedSubject?.replace(subjectUrl, "")?.let { sanitizeTitle(it) }?.ifBlank { null }
            }
        }
    }

    private fun handleImageIntent() {
        sharedImageUri = if (android.os.Build.VERSION.SDK_INT >= android.os.Build.VERSION_CODES.TIRAMISU) {
            intent.getParcelableExtra(Intent.EXTRA_STREAM, Uri::class.java)
        } else {
            @Suppress("DEPRECATION")
            intent.getParcelableExtra(Intent.EXTRA_STREAM)
        }

        if (sharedImageUri == null) {
            Toast.makeText(this, getString(R.string.capture_no_image), Toast.LENGTH_SHORT).show()
            finish()
            return
        }

        try {
            contentResolver.openInputStream(sharedImageUri!!)?.use { stream ->
                imageBitmap = BitmapFactory.decodeStream(stream)
            }
        } catch (e: Exception) {
            Log.e(TAG, "Failed to load image: ${e.message}")
            Toast.makeText(this, getString(R.string.capture_failed_load_image), Toast.LENGTH_SHORT).show()
            finish()
            return
        }

        sharedSubject = intent.getStringExtra(Intent.EXTRA_SUBJECT)?.let { sanitizeTitle(it) }
        // WhatsApp and other apps send captions/descriptions in EXTRA_TEXT
        sharedText = intent.getStringExtra(Intent.EXTRA_TEXT)
    }

    private fun handleFileIntent() {
        sharedFileUri = if (android.os.Build.VERSION.SDK_INT >= android.os.Build.VERSION_CODES.TIRAMISU) {
            intent.getParcelableExtra(Intent.EXTRA_STREAM, Uri::class.java)
        } else {
            @Suppress("DEPRECATION")
            intent.getParcelableExtra(Intent.EXTRA_STREAM)
        }

        if (sharedFileUri == null) {
            Toast.makeText(this, getString(R.string.capture_no_file), Toast.LENGTH_SHORT).show()
            finish()
            return
        }

        // Try to extract video thumbnail
        if (intent.type?.startsWith("video/") == true) {
            try {
                val mmr = MediaMetadataRetriever()
                mmr.setDataSource(this, sharedFileUri)
                fileThumbnail = mmr.getFrameAtTime(500000) // 500ms
                mmr.release()
            } catch (e: Exception) {
                Log.w(TAG, "Could not extract video thumbnail: ${e.message}")
            }
        }

        sharedSubject = intent.getStringExtra(Intent.EXTRA_SUBJECT)?.let { sanitizeTitle(it) }
    }

    /**
     * Handle document files (.html, .json, .tid, .csv) for native TiddlyWiki import.
     * The file is copied to the captures dir and TiddlyWiki's own deserializer + import
     * dialog handles the parsing and tiddler selection.
     */
    private fun handleNativeImportIntent() {
        val streamUri = if (android.os.Build.VERSION.SDK_INT >= android.os.Build.VERSION_CODES.TIRAMISU) {
            intent.getParcelableExtra(Intent.EXTRA_STREAM, Uri::class.java)
        } else {
            @Suppress("DEPRECATION")
            intent.getParcelableExtra(Intent.EXTRA_STREAM)
        }

        if (streamUri == null) {
            Toast.makeText(this, getString(R.string.capture_no_file_import), Toast.LENGTH_SHORT).show()
            finish()
            return
        }

        importFileUri = streamUri
        importFileName = getDisplayName(streamUri) ?: "import"

        // Map to the correct TiddlyWiki deserializer content-type
        importFileMimeType = when {
            importFileName!!.endsWith(".tid") -> "application/x-tiddler"
            importFileName!!.endsWith(".json") -> "application/json"
            importFileName!!.endsWith(".csv") -> "text/csv"
            importFileName!!.endsWith(".html") || importFileName!!.endsWith(".htm") -> "text/html"
            importFileName!!.endsWith(".css") -> "text/css"
            importFileName!!.endsWith(".js") -> "application/javascript"
            else -> intent.type ?: contentResolver.getType(streamUri) ?: "text/plain"
        }

        sharedSubject = importFileName
    }

    private fun getStreamUri(): Uri? {
        return if (android.os.Build.VERSION.SDK_INT >= android.os.Build.VERSION_CODES.TIRAMISU) {
            intent.getParcelableExtra(Intent.EXTRA_STREAM, Uri::class.java)
        } else {
            @Suppress("DEPRECATION")
            intent.getParcelableExtra(Intent.EXTRA_STREAM)
        }
    }

    private fun readTextFromUri(uri: Uri): String? {
        return try {
            contentResolver.openInputStream(uri)?.use { stream ->
                stream.bufferedReader().readText()
            }
        } catch (e: Exception) {
            Log.e(TAG, "Failed to read text from URI: ${e.message}")
            null
        }
    }

    private fun handleSvgIntent() {
        val uri = getStreamUri()
        if (uri == null) {
            Toast.makeText(this, getString(R.string.capture_no_file), Toast.LENGTH_SHORT).show()
            finish()
            return
        }
        val svgText = readTextFromUri(uri)
        if (svgText.isNullOrBlank()) {
            Toast.makeText(this, getString(R.string.capture_failed_read), Toast.LENGTH_SHORT).show()
            finish()
            return
        }
        sharedText = svgText
        sharedTextFileUri = uri
        captureType = "image/svg+xml"
        sharedSubject = getDisplayName(uri)?.substringBeforeLast(".") ?: intent.getStringExtra(Intent.EXTRA_SUBJECT)
    }

    private fun handleMarkdownIntent() {
        val uri = getStreamUri()
        if (uri == null) {
            Toast.makeText(this, getString(R.string.capture_no_file), Toast.LENGTH_SHORT).show()
            finish()
            return
        }
        val mdText = readTextFromUri(uri)
        if (mdText.isNullOrBlank()) {
            Toast.makeText(this, getString(R.string.capture_failed_read), Toast.LENGTH_SHORT).show()
            finish()
            return
        }
        sharedText = mdText
        sharedTextFileUri = uri
        captureType = "text/x-markdown"
        sharedSubject = getDisplayName(uri)?.substringBeforeLast(".") ?: intent.getStringExtra(Intent.EXTRA_SUBJECT)
    }

    private fun handleContactIntent() {
        val uri = getStreamUri()
        if (uri == null) {
            Toast.makeText(this, getString(R.string.capture_no_file), Toast.LENGTH_SHORT).show()
            finish()
            return
        }
        val vcardText = readTextFromUri(uri) ?: ""

        // Unfold vCard continuation lines (lines starting with space or tab are continuations)
        val unfolded = vcardText.replace(Regex("\r?\n[ \t]"), "")

        // Parse vCard fields into wikitext
        val fields = mutableMapOf<String, String>()
        for (line in unfolded.lines()) {
            val parts = line.split(":", limit = 2)
            if (parts.size != 2) continue
            val prefix = parts[0]  // e.g. "FN;CHARSET=UTF-8;ENCODING=QUOTED-PRINTABLE"
            val rawValue = parts[1].trim()
            val params = prefix.split(";")
            val key = params[0].uppercase()

            // Decode value based on encoding parameter
            val value = decodeVcardValue(rawValue, params)

            when (key) {
                "FN" -> fields["Name"] = value
                "N" -> {
                    // N field: Last;First;Middle;Prefix;Suffix
                    if (!fields.containsKey("Name") || fields["Name"].isNullOrBlank()) {
                        val nameParts = value.split(";").map { it.trim() }
                        val assembled = listOfNotNull(
                            nameParts.getOrNull(3)?.ifBlank { null },  // prefix
                            nameParts.getOrNull(1)?.ifBlank { null },  // first
                            nameParts.getOrNull(2)?.ifBlank { null },  // middle
                            nameParts.getOrNull(0)?.ifBlank { null },  // last
                            nameParts.getOrNull(4)?.ifBlank { null }   // suffix
                        ).joinToString(" ")
                        if (assembled.isNotBlank()) fields["Name"] = assembled
                    }
                }
                "TEL" -> fields.merge("Phone", value) { old, new -> "$old, $new" }
                "EMAIL" -> fields.merge("Email", value) { old, new -> "$old, $new" }
                "ORG" -> fields["Organization"] = value.replace(";", ", ")
                "TITLE" -> fields["Job Title"] = value
                "ADR" -> {
                    val addr = value.replace(";", " ").replace(Regex("\\s+"), " ").trim()
                    if (addr.isNotBlank()) fields["Address"] = addr
                }
                "URL" -> fields.merge("URL", value) { old, new -> "$old, $new" }
                "NOTE" -> fields["Note"] = value
                "BDAY" -> fields["Birthday"] = value
            }
        }
        val wikitext = buildString {
            fields.forEach { (key, value) ->
                when (key) {
                    "URL" -> appendLine("|$key|[[$value]]|")
                    "Email" -> {
                        val emails = value.split(",").map { it.trim() }
                        val links = emails.joinToString(", ") { "[ext[${it}|mailto:${it}]]" }
                        appendLine("|$key|$links|")
                    }
                    "Phone" -> {
                        val phones = value.split(",").map { it.trim() }
                        val links = phones.joinToString(", ") { "[ext[${it}|tel:${it}]]" }
                        appendLine("|$key|$links|")
                    }
                    else -> appendLine("|$key|$value|")
                }
            }
        }
        sharedText = wikitext.ifBlank { vcardText }
        sharedSubject = fields["Name"] ?: getDisplayName(uri)?.substringBeforeLast(".")
    }

    /**
     * Decode a vCard property value based on encoding parameters.
     * Handles QUOTED-PRINTABLE and BASE64 encodings.
     */
    private fun decodeVcardValue(raw: String, params: List<String>): String {
        val upperParams = params.map { it.uppercase() }
        val isQP = upperParams.any { it.contains("QUOTED-PRINTABLE") }
        val isBase64 = upperParams.any { it.contains("BASE64") || it.contains("ENCODING=B") }
        val charset = upperParams.firstOrNull { it.startsWith("CHARSET=") }
            ?.substringAfter("CHARSET=") ?: "UTF-8"

        return when {
            isQP -> decodeQuotedPrintable(raw, charset)
            isBase64 -> try {
                String(android.util.Base64.decode(raw, android.util.Base64.DEFAULT),
                    java.nio.charset.Charset.forName(charset))
            } catch (_: Exception) { raw }
            else -> raw
        }
    }

    /**
     * Decode a quoted-printable encoded string.
     * =XX is a hex-encoded byte, =\n is a soft line break.
     */
    private fun decodeQuotedPrintable(input: String, charset: String = "UTF-8"): String {
        val bytes = mutableListOf<Byte>()
        var i = 0
        while (i < input.length) {
            val c = input[i]
            if (c == '=' && i + 2 < input.length) {
                val hex = input.substring(i + 1, i + 3)
                if (hex == "\r\n" || hex.startsWith("\n")) {
                    // Soft line break — skip
                    i += if (hex == "\r\n") 3 else 2
                    continue
                }
                try {
                    bytes.add(hex.toInt(16).toByte())
                    i += 3
                    continue
                } catch (_: NumberFormatException) {
                    // Not valid QP, treat as literal
                }
            }
            bytes.add(c.code.toByte())
            i++
        }
        return try {
            String(bytes.toByteArray(), java.nio.charset.Charset.forName(charset))
        } catch (_: Exception) {
            String(bytes.toByteArray(), Charsets.UTF_8)
        }
    }

    private fun handleCalendarIntent() {
        val uri = getStreamUri()
        if (uri == null) {
            Toast.makeText(this, getString(R.string.capture_no_file), Toast.LENGTH_SHORT).show()
            finish()
            return
        }
        val icsText = readTextFromUri(uri) ?: ""
        // Parse iCalendar VEVENT fields
        val fields = mutableMapOf<String, String>()
        var inEvent = false
        for (line in icsText.lines()) {
            val trimmed = line.trim()
            if (trimmed == "BEGIN:VEVENT") inEvent = true
            if (trimmed == "END:VEVENT") inEvent = false
            if (!inEvent) continue
            val parts = trimmed.split(":", limit = 2)
            if (parts.size == 2) {
                val key = parts[0].split(";")[0].uppercase()
                val value = parts[1].trim()
                when (key) {
                    "SUMMARY" -> fields["Summary"] = value
                    "DTSTART" -> fields["Start"] = formatIcsDate(value)
                    "DTEND" -> fields["End"] = formatIcsDate(value)
                    "LOCATION" -> fields["Location"] = value
                    "DESCRIPTION" -> fields["Description"] = value.replace("\\n", "\n").replace("\\,", ",")
                    "URL" -> fields["URL"] = value
                    "ORGANIZER" -> {
                        val email = value.removePrefix("mailto:").removePrefix("MAILTO:")
                        fields["Organizer"] = email
                    }
                }
            }
        }
        val wikitext = buildString {
            fields.forEach { (key, value) ->
                when (key) {
                    "Description" -> {
                        appendLine("")
                        appendLine(value)
                    }
                    "URL" -> appendLine("|$key|[[$value]]|")
                    else -> appendLine("|$key|$value|")
                }
            }
        }
        sharedText = wikitext.ifBlank { icsText }
        sharedSubject = fields["Summary"] ?: getDisplayName(uri)?.substringBeforeLast(".")
    }

    private fun formatIcsDate(value: String): String {
        // Convert 20260211T100000Z or 20260211 to readable format
        return try {
            val clean = value.replace("Z", "").replace("T", "")
            val year = clean.substring(0, 4)
            val month = clean.substring(4, 6)
            val day = clean.substring(6, 8)
            if (clean.length >= 12) {
                val hour = clean.substring(8, 10)
                val min = clean.substring(10, 12)
                "$year-$month-$day $hour:$min"
            } else {
                "$year-$month-$day"
            }
        } catch (e: Exception) { value }
    }

    private fun handleMultipleIntent() {
        val uris = if (android.os.Build.VERSION.SDK_INT >= android.os.Build.VERSION_CODES.TIRAMISU) {
            intent.getParcelableArrayListExtra(Intent.EXTRA_STREAM, Uri::class.java)
        } else {
            @Suppress("DEPRECATION")
            intent.getParcelableArrayListExtra<Uri>(Intent.EXTRA_STREAM)
        }

        if (uris.isNullOrEmpty()) {
            Toast.makeText(this, getString(R.string.capture_no_file), Toast.LENGTH_SHORT).show()
            finish()
            return
        }

        multipleUris = uris
        sharedSubject = intent.getStringExtra(Intent.EXTRA_SUBJECT)?.let { sanitizeTitle(it) }

        // Load thumbnails for first few items (max 4)
        val thumbs = mutableListOf<Bitmap?>()
        for (uri in uris.take(4)) {
            val mimeType = contentResolver.getType(uri) ?: ""
            var thumb: Bitmap? = null
            if (mimeType.startsWith("image/")) {
                try {
                    contentResolver.openInputStream(uri)?.use { stream ->
                        val opts = BitmapFactory.Options().apply { inSampleSize = 4 }
                        thumb = BitmapFactory.decodeStream(stream, null, opts)
                    }
                } catch (_: Exception) {}
            } else if (mimeType.startsWith("video/")) {
                try {
                    val mmr = MediaMetadataRetriever()
                    mmr.setDataSource(this, uri)
                    thumb = mmr.getFrameAtTime(500000)
                    mmr.release()
                } catch (_: Exception) {}
            }
            thumbs.add(thumb)
        }
        multipleThumbnails = thumbs
    }

    private fun loadRecentWikis(): List<WikiEntry> {
        val wikis = mutableListOf<WikiEntry>()

        // Load the authoritative Rust-side wiki list paths for cross-referencing
        val rustPaths = mutableSetOf<String>()
        val rustFile = File(filesDir.parentFile, "recent_wikis.json")
        if (rustFile.exists()) {
            try {
                val arr = JSONArray(rustFile.readText())
                for (i in 0 until arr.length()) {
                    val obj = arr.optJSONObject(i) ?: continue
                    val path = obj.optString("path", "")
                    if (path.isNotEmpty()) rustPaths.add(path)
                }
            } catch (e: Exception) {
                Log.w(TAG, "Failed to read Rust recent_wikis.json: ${e.message}")
            }
        }

        // Try Kotlin-side recent_wikis.json first (written by WikiActivity.updateRecentWikis)
        val kotlinFile = File(filesDir, "recent_wikis.json")
        if (kotlinFile.exists()) {
            try {
                val arr = JSONArray(kotlinFile.readText())
                for (i in 0 until arr.length()) {
                    val obj = arr.optJSONObject(i) ?: continue
                    val path = obj.optString("path", "")
                    val title = obj.optString("title", "")
                    val isFolder = obj.optBoolean("is_folder", false)
                    val externalAttachments = obj.optBoolean("external_attachments", true)
                    if (path.isNotEmpty() && title.isNotEmpty()) {
                        // Only include if still present in the authoritative Rust-side list
                        if (rustPaths.isEmpty() || rustPaths.contains(path)) {
                            wikis.add(WikiEntry(path, title, isFolder, externalAttachments))
                        }
                    }
                }
            } catch (e: Exception) {
                Log.w(TAG, "Failed to read Kotlin recent_wikis.json: ${e.message}")
            }
        }

        if (wikis.isEmpty() && rustPaths.isNotEmpty()) {
            // Fall back to Rust-side recent_wikis.json
            try {
                val arr = JSONArray(rustFile.readText())
                for (i in 0 until arr.length()) {
                    val obj = arr.optJSONObject(i) ?: continue
                    val path = obj.optString("path", "")
                    val filename = obj.optString("filename", "")
                    val isFolder = obj.optBoolean("is_folder", false)
                    if (path.isNotEmpty()) {
                        wikis.add(WikiEntry(path, filename.ifEmpty { "Wiki" }, isFolder))
                    }
                }
            } catch (e: Exception) {
                Log.w(TAG, "Failed to read Rust recent_wikis.json (fallback): ${e.message}")
            }
        }

        return wikis
    }

    private fun dp(value: Int): Int {
        return TypedValue.applyDimension(
            TypedValue.COMPLEX_UNIT_DIP, value.toFloat(), resources.displayMetrics
        ).toInt()
    }

    // Material Design color palette (resolved from theme for DayNight support)
    private var colorPrimary = 0
    private var colorOnPrimary = 0
    private var colorPrimaryContainer = 0
    private var colorOnPrimaryContainer = 0
    private var colorSurface = 0
    private var colorSurfaceVariant = 0
    private var colorOnSurface = 0
    private var colorOnSurfaceVariant = 0
    private var colorOutline = 0
    private var colorOutlineVariant = 0
    private var colorError = 0
    private val colorScrim = 0x52000000.toInt()          // 32% black (works for both modes)

    private fun resolveThemeColor(attr: Int, fallback: Int): Int {
        val tv = TypedValue()
        return if (theme.resolveAttribute(attr, tv, true)) tv.data else fallback
    }

    private fun resolveThemeColors() {
        colorPrimary = resolveThemeColor(com.google.android.material.R.attr.colorPrimary, 0xFF6750A4.toInt())
        colorOnPrimary = resolveThemeColor(com.google.android.material.R.attr.colorOnPrimary, 0xFFFFFFFF.toInt())
        colorSurface = resolveThemeColor(com.google.android.material.R.attr.colorSurface, 0xFFFFFBFE.toInt())
        colorOnSurface = resolveThemeColor(com.google.android.material.R.attr.colorOnSurface, 0xFF1C1B1F.toInt())
        colorError = resolveThemeColor(com.google.android.material.R.attr.colorError, 0xFFB3261E.toInt())
        colorSurfaceVariant = resolveThemeColor(com.google.android.material.R.attr.colorSurfaceVariant, 0xFFE7E0EC.toInt())
        colorOnSurfaceVariant = resolveThemeColor(com.google.android.material.R.attr.colorOnSurfaceVariant, 0xFF49454F.toInt())
        colorOutline = resolveThemeColor(com.google.android.material.R.attr.colorOutline, 0xFF79747E.toInt())
        colorOutlineVariant = resolveThemeColor(com.google.android.material.R.attr.colorOutlineVariant, 0xFFCAC4D0.toInt())
        colorPrimaryContainer = resolveThemeColor(com.google.android.material.R.attr.colorPrimaryContainer, 0xFFEADDFF.toInt())
        colorOnPrimaryContainer = resolveThemeColor(com.google.android.material.R.attr.colorOnPrimaryContainer, 0xFF21005D.toInt())
    }

    private fun makeOutlinedFieldBg(): GradientDrawable {
        return GradientDrawable().apply {
            setColor(colorSurface)
            cornerRadius = dp(12).toFloat()
            setStroke(dp(1), colorOutline)
        }
    }

    private fun makeRoundedClip(radiusDp: Int): ViewOutlineProvider {
        return object : ViewOutlineProvider() {
            override fun getOutline(view: View, outline: Outline) {
                outline.setRoundRect(0, 0, view.width, view.height, dp(radiusDp).toFloat())
            }
        }
    }

    private fun buildUI() {
        // Scrim overlay
        val overlay = FrameLayout(this).apply {
            setBackgroundColor(colorScrim)
            setOnClickListener { finish() }
        }

        // Scrollable card container
        val scrollView = ScrollView(this).apply {
            isVerticalScrollBarEnabled = false
            setOnClickListener { /* consume */ }
        }

        // Card with elevation
        val cardBg = GradientDrawable().apply {
            setColor(colorSurface)
            cornerRadius = dp(28).toFloat()
        }

        val card = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            background = cardBg
            setPadding(dp(24), dp(24), dp(24), dp(24))
            isClickable = true
            elevation = dp(6).toFloat()
        }

        // Header
        card.addView(TextView(this).apply {
            text = getString(R.string.capture_title)
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 22f)
            setTextColor(colorOnSurface)
            setTypeface(typeface, android.graphics.Typeface.BOLD)
            letterSpacing = -0.01f
        }, LinearLayout.LayoutParams(
            ViewGroup.LayoutParams.MATCH_PARENT,
            ViewGroup.LayoutParams.WRAP_CONTENT
        ).apply { bottomMargin = dp(16) })

        // Text preview
        if (sharedText != null) {
            val previewBg = GradientDrawable().apply {
                setColor(colorSurfaceVariant)
                cornerRadius = dp(12).toFloat()
            }
            card.addView(TextView(this).apply {
                text = sharedText
                maxLines = 5
                ellipsize = android.text.TextUtils.TruncateAt.END
                background = previewBg
                setPadding(dp(16), dp(12), dp(16), dp(12))
                setTextColor(colorOnSurfaceVariant)
                setTextSize(TypedValue.COMPLEX_UNIT_SP, 14f)
                setLineSpacing(0f, 1.3f)
            }, LinearLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT,
                ViewGroup.LayoutParams.WRAP_CONTENT
            ).apply { bottomMargin = dp(16) })
        }

        // Editable content field for blank captures (from Quick Capture widget)
        if (isBlankCapture) {
            card.addView(TextView(this).apply {
                text = getString(R.string.label_content)
                setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
                setTextColor(colorOnSurfaceVariant)
            }, LinearLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT,
                ViewGroup.LayoutParams.WRAP_CONTENT
            ).apply { bottomMargin = dp(4) })

            contentEdit = EditText(this).apply {
                hint = getString(R.string.hint_content)
                background = makeOutlinedFieldBg()
                setPadding(dp(16), dp(12), dp(16), dp(12))
                setTextColor(colorOnSurface)
                setHintTextColor(colorOnSurfaceVariant)
                setTextSize(TypedValue.COMPLEX_UNIT_SP, 14f)
                inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_MULTI_LINE
                minLines = 3
                maxLines = 8
                gravity = Gravity.TOP
            }
            card.addView(contentEdit, LinearLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT,
                ViewGroup.LayoutParams.WRAP_CONTENT
            ).apply { bottomMargin = dp(16) })
        }

        // Image preview with rounded corners
        if (imageBitmap != null) {
            card.addView(ImageView(this).apply {
                setImageBitmap(imageBitmap)
                scaleType = ImageView.ScaleType.CENTER_CROP
                adjustViewBounds = true
                outlineProvider = makeRoundedClip(16)
                clipToOutline = true
            }, LinearLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT,
                dp(200)
            ).apply { bottomMargin = dp(16) })
        }

        // File preview (video/audio) with rounded thumbnail
        if (sharedFileUri != null) {
            if (fileThumbnail != null) {
                card.addView(ImageView(this).apply {
                    setImageBitmap(fileThumbnail)
                    scaleType = ImageView.ScaleType.CENTER_CROP
                    adjustViewBounds = true
                    outlineProvider = makeRoundedClip(16)
                    clipToOutline = true
                }, LinearLayout.LayoutParams(
                    ViewGroup.LayoutParams.MATCH_PARENT,
                    dp(200)
                ).apply { bottomMargin = dp(8) })
            }
            val name = getDisplayName(sharedFileUri!!) ?: "File"
            val mimeType = contentResolver.getType(sharedFileUri!!) ?: intent.type ?: "unknown"
            card.addView(TextView(this).apply {
                text = "$name ($mimeType)"
                setTextColor(colorOnSurfaceVariant)
                setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
            }, LinearLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT,
                ViewGroup.LayoutParams.WRAP_CONTENT
            ).apply { bottomMargin = dp(16) })
        }

        // Import file preview (.html, .json, .tid, .csv)
        if (importFileUri != null) {
            val previewBg = GradientDrawable().apply {
                setColor(colorSurfaceVariant)
                cornerRadius = dp(12).toFloat()
            }
            card.addView(TextView(this).apply {
                text = "$importFileName\nType: $importFileMimeType\n\n${getString(R.string.capture_import_note)}"
                background = previewBg
                setPadding(dp(16), dp(12), dp(16), dp(12))
                setTextColor(colorOnSurfaceVariant)
                setTextSize(TypedValue.COMPLEX_UNIT_SP, 14f)
                setLineSpacing(0f, 1.3f)
            }, LinearLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT,
                ViewGroup.LayoutParams.WRAP_CONTENT
            ).apply { bottomMargin = dp(16) })
        }

        // Multiple items preview (SEND_MULTIPLE)
        if (multipleUris != null) {
            val count = multipleUris!!.size
            // Thumbnail row
            if (multipleThumbnails.isNotEmpty()) {
                val thumbRow = LinearLayout(this).apply {
                    orientation = LinearLayout.HORIZONTAL
                    gravity = Gravity.START
                }
                for (thumb in multipleThumbnails) {
                    if (thumb != null) {
                        thumbRow.addView(ImageView(this).apply {
                            setImageBitmap(thumb)
                            scaleType = ImageView.ScaleType.CENTER_CROP
                            outlineProvider = makeRoundedClip(12)
                            clipToOutline = true
                        }, LinearLayout.LayoutParams(dp(80), dp(80)).apply {
                            rightMargin = dp(8)
                        })
                    }
                }
                if (count > 4) {
                    thumbRow.addView(TextView(this).apply {
                        text = "+${count - 4}"
                        setTextColor(colorOnSurfaceVariant)
                        setTextSize(TypedValue.COMPLEX_UNIT_SP, 16f)
                        gravity = Gravity.CENTER
                    }, LinearLayout.LayoutParams(dp(80), dp(80)))
                }
                card.addView(thumbRow, LinearLayout.LayoutParams(
                    ViewGroup.LayoutParams.MATCH_PARENT,
                    ViewGroup.LayoutParams.WRAP_CONTENT
                ).apply { bottomMargin = dp(8) })
            }
            card.addView(TextView(this).apply {
                text = getString(R.string.capture_multiple_items, count)
                setTextColor(colorOnSurfaceVariant)
                setTextSize(TypedValue.COMPLEX_UNIT_SP, 13f)
            }, LinearLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT,
                ViewGroup.LayoutParams.WRAP_CONTENT
            ).apply { bottomMargin = dp(16) })
        }

        // URL preview chip
        if (detectedUrl != null) {
            val urlBg = GradientDrawable().apply {
                setColor(0xFFE8F0FE.toInt())
                cornerRadius = dp(8).toFloat()
            }
            card.addView(TextView(this).apply {
                text = detectedUrl
                setTextColor(0xFF1A73E8.toInt())
                setTextSize(TypedValue.COMPLEX_UNIT_SP, 13f)
                maxLines = 2
                ellipsize = android.text.TextUtils.TruncateAt.END
                background = urlBg
                setPadding(dp(12), dp(8), dp(12), dp(8))
            }, LinearLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT,
                ViewGroup.LayoutParams.WRAP_CONTENT
            ).apply { bottomMargin = dp(16) })
        }

        // Title field with outlined style (hidden for multiple items — each uses its filename)
        if (multipleUris == null) {
            card.addView(TextView(this).apply {
                text = getString(R.string.label_title)
                setTextColor(colorOnSurfaceVariant)
                setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
                letterSpacing = 0.04f
            }, LinearLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT,
                ViewGroup.LayoutParams.WRAP_CONTENT
            ).apply { bottomMargin = dp(4) })

            titleEdit = EditText(this).apply {
                inputType = InputType.TYPE_CLASS_TEXT
                setSingleLine(true)
                background = makeOutlinedFieldBg()
                setPadding(dp(16), dp(12), dp(16), dp(12))
                if (importFileUri != null) {
                    setText(importFileName?.trimEnd('.') ?: "import")
                } else {
                    // When there's a URL with remaining text, use it as title — it's
                    // almost always more meaningful than the raw URL.
                    val textTitle = if (detectedUrl != null && !sharedText.isNullOrBlank()) {
                        sanitizeTitle(sharedText!!).take(100).ifBlank { null }
                    } else null
                    val defaultTitle = sharedSubject
                        ?: textTitle
                        ?: detectedUrl
                        ?: (sharedFileUri ?: sharedImageUri)?.let { getDisplayName(it)?.substringBeforeLast(".") }
                        ?: ""
                    setText(defaultTitle)
                }
                setTextColor(colorOnSurface)
                setHintTextColor(colorOutline)
                hint = getString(R.string.hint_tiddler_title)
                setTextSize(TypedValue.COMPLEX_UNIT_SP, 15f)
                setOnFocusChangeListener { _, hasFocus ->
                    val bg = background as? GradientDrawable ?: return@setOnFocusChangeListener
                    bg.setStroke(dp(if (hasFocus) 2 else 1), if (hasFocus) colorPrimary else colorOutline)
                }
            }
            card.addView(titleEdit, LinearLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT,
                ViewGroup.LayoutParams.WRAP_CONTENT
            ).apply { bottomMargin = dp(16) })
        }

        // Tags field with outlined style (hidden for imports)
        val tagsLabel = TextView(this).apply {
            text = getString(R.string.label_tags)
            setTextColor(colorOnSurfaceVariant)
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
            letterSpacing = 0.04f
            if (importFileUri != null) visibility = View.GONE
        }
        card.addView(tagsLabel, LinearLayout.LayoutParams(
            ViewGroup.LayoutParams.MATCH_PARENT,
            ViewGroup.LayoutParams.WRAP_CONTENT
        ).apply { bottomMargin = dp(4) })

        tagsEdit = EditText(this).apply {
            inputType = InputType.TYPE_CLASS_TEXT
            setSingleLine(true)
            background = makeOutlinedFieldBg()
            setPadding(dp(16), dp(12), dp(16), dp(12))
            setText("")
            setTextColor(colorOnSurface)
            setHintTextColor(colorOutline)
            hint = getString(R.string.hint_tags)
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 15f)
            if (importFileUri != null) visibility = View.GONE
            setOnFocusChangeListener { _, hasFocus ->
                val bg = background as? GradientDrawable ?: return@setOnFocusChangeListener
                bg.setStroke(dp(if (hasFocus) 2 else 1), if (hasFocus) colorPrimary else colorOutline)
            }
        }
        card.addView(tagsEdit, LinearLayout.LayoutParams(
            ViewGroup.LayoutParams.MATCH_PARENT,
            ViewGroup.LayoutParams.WRAP_CONTENT
        ).apply { bottomMargin = dp(16) })

        // Wiki selector label
        card.addView(TextView(this).apply {
            text = getString(R.string.label_save_to)
            setTextColor(colorOnSurfaceVariant)
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
            letterSpacing = 0.04f
        }, LinearLayout.LayoutParams(
            ViewGroup.LayoutParams.MATCH_PARENT,
            ViewGroup.LayoutParams.WRAP_CONTENT
        ).apply { bottomMargin = dp(4) })

        if (wikiList.isEmpty()) {
            // No wikis found
            val errorBg = GradientDrawable().apply {
                setColor(0xFFFCE4EC.toInt())
                cornerRadius = dp(12).toFloat()
            }
            card.addView(TextView(this).apply {
                text = getString(R.string.capture_no_wikis)
                setTextColor(colorError)
                setTextSize(TypedValue.COMPLEX_UNIT_SP, 14f)
                background = errorBg
                setPadding(dp(16), dp(12), dp(16), dp(12))
            }, LinearLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT,
                ViewGroup.LayoutParams.WRAP_CONTENT
            ).apply { bottomMargin = dp(20) })

            // Cancel only — text button style
            card.addView(MaterialButton(this, null, com.google.android.material.R.attr.materialButtonOutlinedStyle).apply {
                text = getString(R.string.btn_cancel)
                setOnClickListener { finish() }
                cornerRadius = dp(20)
            }, LinearLayout.LayoutParams(
                ViewGroup.LayoutParams.WRAP_CONTENT,
                ViewGroup.LayoutParams.WRAP_CONTENT
            ).apply { gravity = Gravity.END })
        } else {
            // Wiki spinner with outlined background
            val spinnerBg = makeOutlinedFieldBg()
            wikiSpinner = Spinner(this).apply {
                val wikiTitles = wikiList.map { it.title }.toTypedArray()
                adapter = ArrayAdapter(this@CaptureActivity, android.R.layout.simple_spinner_dropdown_item, wikiTitles)
                background = spinnerBg
                setPadding(dp(12), dp(8), dp(12), dp(8))
                // Restore last selected wiki
                val lastPath = getSharedPreferences("capture_prefs", MODE_PRIVATE)
                    .getString("last_wiki_path", null)
                if (lastPath != null) {
                    val idx = wikiList.indexOfFirst { it.path == lastPath }
                    if (idx >= 0) setSelection(idx)
                }
            }
            card.addView(wikiSpinner, LinearLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT,
                ViewGroup.LayoutParams.WRAP_CONTENT
            ).apply { bottomMargin = dp(20) })

            // Progress indicator for auto meta preview fetch
            if (detectedUrl != null) {
                clipProgress = ProgressBar(this).apply {
                    visibility = View.GONE
                    if (android.os.Build.VERSION.SDK_INT >= android.os.Build.VERSION_CODES.LOLLIPOP) {
                        indeterminateTintList = ColorStateList.valueOf(colorPrimary)
                    }
                }
                card.addView(clipProgress, LinearLayout.LayoutParams(
                    ViewGroup.LayoutParams.WRAP_CONTENT,
                    ViewGroup.LayoutParams.WRAP_CONTENT
                ).apply { bottomMargin = dp(16); gravity = Gravity.CENTER_HORIZONTAL })
            }

            // Button row
            val buttonRow = LinearLayout(this).apply {
                orientation = LinearLayout.HORIZONTAL
                gravity = Gravity.END
            }

            // Cancel — outlined button
            buttonRow.addView(MaterialButton(this, null, com.google.android.material.R.attr.materialButtonOutlinedStyle).apply {
                text = getString(R.string.btn_cancel)
                setOnClickListener { finish() }
                cornerRadius = dp(20)
                setStrokeColorResource(android.R.color.transparent)
                setTextColor(colorPrimary)
            }, LinearLayout.LayoutParams(
                ViewGroup.LayoutParams.WRAP_CONTENT,
                ViewGroup.LayoutParams.WRAP_CONTENT
            ).apply { rightMargin = dp(8) })

            // Save — filled primary button
            buttonRow.addView(MaterialButton(this).apply {
                text = getString(R.string.btn_save)
                setOnClickListener { saveCapture() }
                cornerRadius = dp(20)
                setBackgroundColor(colorPrimary)
                setTextColor(colorOnPrimary)
                elevation = dp(2).toFloat()
            })

            card.addView(buttonRow, LinearLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT,
                ViewGroup.LayoutParams.WRAP_CONTENT
            ))
        }

        scrollView.addView(card)

        val scrollParams = FrameLayout.LayoutParams(
            ViewGroup.LayoutParams.MATCH_PARENT,
            ViewGroup.LayoutParams.WRAP_CONTENT
        ).apply {
            gravity = Gravity.CENTER
            val maxWidthPx = dp(480)
            val screenWidth = resources.displayMetrics.widthPixels
            val margin = dp(24)
            if (screenWidth > maxWidthPx + margin * 2) {
                leftMargin = (screenWidth - maxWidthPx) / 2
                rightMargin = (screenWidth - maxWidthPx) / 2
            } else {
                leftMargin = margin
                rightMargin = margin
            }
            topMargin = margin
            bottomMargin = margin
        }
        overlay.addView(scrollView, scrollParams)

        setContentView(overlay)
    }

    private fun startWebClip() {
        val url = detectedUrl ?: return
        clipProgress?.visibility = android.view.View.VISIBLE

        Thread {
            val result = fetchAndParseWebPage(url)
            runOnUiThread {
                clipProgress?.visibility = android.view.View.GONE

                if (result != null) {
                    clippedContent = result.second
                    // Always update title from web clip — it's more accurate than
                    // EXTRA_SUBJECT (which may be empty, a URL, or have wrong encoding)
                    val clippedTitle = sanitizeTitle(result.first)
                    if (clippedTitle.isNotBlank() && !clippedTitle.startsWith("http")) {
                        titleEdit?.setText(clippedTitle)
                    }
                }
                // On failure, save will fall back to [[title|url]] bookmark
            }
        }.start()
    }

    /**
     * Extract structured data from JSON-LD (<script type="application/ld+json">).
     * Returns a map with keys: description, author, datePublished, image.
     * Handles top-level objects, arrays, and @graph arrays.
     * Prioritizes content types (Article, NewsArticle, etc.) over structural types.
     */
    private fun extractJsonLd(html: String): Map<String, String> {
        val result = mutableMapOf<String, String>()
        val blocks = Regex(
            """<script\s+type\s*=\s*["']application/ld\+json["'][^>]*>(.*?)</script>""",
            setOf(RegexOption.DOT_MATCHES_ALL, RegexOption.IGNORE_CASE)
        ).findAll(html)

        // Content types we care about (prioritized over structural types)
        val contentTypes = setOf(
            "Article", "NewsArticle", "BlogPosting", "TechArticle", "ScholarlyArticle",
            "WebPage", "ItemPage", "FAQPage", "HowTo", "Recipe", "Review",
            "Product", "SocialMediaPosting", "DiscussionForumPosting", "Report"
        )
        // Structural types we skip when better alternatives exist
        val structuralTypes = setOf(
            "Organization", "BreadcrumbList", "WebSite", "SearchAction",
            "SiteNavigationElement", "WPHeader", "WPFooter", "ImageObject"
        )

        // Collect all JSON-LD objects, flattening @graph arrays
        val allObjects = mutableListOf<JSONObject>()

        for (match in blocks) {
            try {
                val jsonStr = match.groupValues[1].trim()
                if (jsonStr.startsWith("[")) {
                    // Top-level JSONArray
                    val arr = JSONArray(jsonStr)
                    for (i in 0 until arr.length()) {
                        arr.optJSONObject(i)?.let { allObjects.add(it) }
                    }
                } else {
                    val json = JSONObject(jsonStr)
                    // Check for @graph array
                    val graph = json.optJSONArray("@graph")
                    if (graph != null) {
                        for (i in 0 until graph.length()) {
                            graph.optJSONObject(i)?.let { allObjects.add(it) }
                        }
                    } else {
                        allObjects.add(json)
                    }
                }
            } catch (e: Exception) {
                // Skip malformed JSON-LD blocks
            }
        }

        // Sort: content types first, then unknown types, structural types last
        val sorted = allObjects.sortedBy { obj ->
            val type = obj.optString("@type", "")
            when {
                contentTypes.any { type.contains(it, ignoreCase = true) } -> 0
                structuralTypes.any { type.contains(it, ignoreCase = true) } -> 2
                else -> 1
            }
        }

        // Extract from each object, later (higher-priority) values overwrite earlier ones
        for (json in sorted) {
            extractFromJsonLdObject(json, result)
        }

        return result
    }

    /**
     * Extract metadata fields from a single JSON-LD object into the result map.
     * Only overwrites existing values if the new value is longer (better quality).
     */
    private fun extractFromJsonLdObject(json: JSONObject, result: MutableMap<String, String>) {
        // Description: text > articleBody > description
        val desc = json.optString("text", "").ifBlank {
            json.optString("articleBody", "").ifBlank {
                json.optString("description", "")
            }
        }
        if (desc.isNotBlank()) {
            val clean = Regex("<[^>]+>").replace(desc, "")
                .replace(Regex("\\s+"), " ").trim().take(800)
            if (clean.isNotBlank()) {
                val existing = result["description"] ?: ""
                if (clean.length > existing.length) result["description"] = clean
            }
        }

        // Author — can be a string, object, or array of objects
        val authorName = when {
            json.has("author") && json.opt("author") is JSONArray -> {
                val arr = json.getJSONArray("author")
                (0 until arr.length()).mapNotNull { i ->
                    arr.optJSONObject(i)?.optString("name", "")?.takeIf { it.isNotBlank() }
                }.joinToString(", ")
            }
            json.optJSONObject("author") != null -> {
                json.getJSONObject("author").optString("name", "")
            }
            else -> json.optString("author", "")
        }
        if (authorName.isNotBlank()) result["author"] = authorName

        // Date
        val date = json.optString("datePublished", "").ifBlank {
            json.optString("dateCreated", "")
        }
        if (date.isNotBlank()) result["datePublished"] = date

        // Image — can be a string, object with url, or array
        val imageUrl = when {
            json.optJSONArray("image") != null -> {
                val arr = json.getJSONArray("image")
                // Array of strings or objects
                var url = ""
                for (i in 0 until arr.length()) {
                    val item = arr.opt(i)
                    url = when (item) {
                        is String -> item
                        is JSONObject -> item.optString("url", "")
                        else -> ""
                    }
                    if (url.startsWith("http")) break
                }
                url
            }
            json.optJSONObject("image") != null -> {
                json.getJSONObject("image").optString("url", "")
            }
            else -> json.optString("image", "")
        }
        if (imageUrl.isNotBlank() && imageUrl.startsWith("http")) result["image"] = imageUrl
    }

    /**
     * Fetch a web page's meta preview and build a wikitext card.
     * Uses JSON-LD structured data when available (more specific for sub-pages/posts),
     * falls back to OG/meta tags. Does NOT extract body content.
     * Returns Pair(title, wikitextContent) or null on failure.
     */
    /**
     * Fetch a URL, following HTTP and HTML/JS redirects. Returns Pair(finalUrl, rawBytes, httpCharset).
     */
    private fun fetchUrl(urlString: String): Triple<String, ByteArray, String?>? {
        var currentUrl = urlString
        var maxRedirects = 8

        while (maxRedirects > 0) {
            val url = URL(currentUrl)
            val conn = url.openConnection() as HttpURLConnection
            conn.connectTimeout = 10000
            conn.readTimeout = 10000
            conn.instanceFollowRedirects = false
            conn.useCaches = false
            conn.setRequestProperty("Cache-Control", "no-cache")
            conn.setRequestProperty("User-Agent",
                "Mozilla/5.0 (Linux; Android 14) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Mobile Safari/537.36")
            conn.setRequestProperty("Accept", "text/html,application/xhtml+xml,*/*")
            conn.setRequestProperty("Accept-Language", "en-US,en;q=0.9,de;q=0.8")

            val responseCode = conn.responseCode

            // HTTP redirect (301-308)
            if (responseCode in 301..308) {
                val location = conn.getHeaderField("Location")
                conn.disconnect()
                if (location.isNullOrBlank()) return null
                currentUrl = if (location.startsWith("http")) location
                             else URL(URL(currentUrl), location).toString()
                maxRedirects--
                Log.d(TAG, "Web clip: HTTP redirect → $currentUrl")
                continue
            }

            if (responseCode != 200) {
                Log.w(TAG, "Web clip failed: HTTP $responseCode for $currentUrl")
                conn.disconnect()
                return null
            }

            val contentType = conn.contentType ?: ""
            val httpCharset = if (contentType.contains("charset=", ignoreCase = true)) {
                contentType.substringAfter("charset=").substringBefore(";").trim()
                    .removeSurrounding("\"").removeSurrounding("'").trim()
            } else null

            val rawBytes = conn.inputStream.readBytes()
            conn.disconnect()

            // Check for HTML meta-refresh or JS redirects in small/redirect pages
            if (rawBytes.size < 10000) {
                val preview = String(rawBytes, Charsets.UTF_8)
                // <meta http-equiv="refresh" content="0;url=...">
                val metaRefresh = Regex(
                    """<meta\s[^>]*http-equiv\s*=\s*["']?refresh["']?\s[^>]*content\s*=\s*["'][^"']*url=([^"'\s>]+)""",
                    RegexOption.IGNORE_CASE
                ).find(preview)
                    ?: Regex(
                        """<meta\s[^>]*content\s*=\s*["'][^"']*url=([^"'\s>]+)["'][^>]*http-equiv\s*=\s*["']?refresh""",
                        RegexOption.IGNORE_CASE
                    ).find(preview)
                if (metaRefresh != null) {
                    val target = metaRefresh.groupValues[1].trim()
                    if (target.startsWith("http")) {
                        currentUrl = target
                        maxRedirects--
                        Log.d(TAG, "Web clip: meta-refresh redirect → $currentUrl")
                        continue
                    }
                }
                // window.location = "..." or window.location.href = "..."
                val jsRedirect = Regex(
                    """(?:window\.location(?:\.href)?\s*=\s*|window\.location\.replace\s*\(\s*)["']([^"']+)["']""",
                    RegexOption.IGNORE_CASE
                ).find(preview)
                if (jsRedirect != null) {
                    val target = jsRedirect.groupValues[1].trim()
                    if (target.startsWith("http")) {
                        currentUrl = target
                        maxRedirects--
                        Log.d(TAG, "Web clip: JS redirect → $currentUrl")
                        continue
                    }
                }
            }

            Log.d(TAG, "Web clip: final URL=$currentUrl, ${rawBytes.size} bytes, charset=$httpCharset")
            return Triple(currentUrl, rawBytes, httpCharset)
        }

        Log.w(TAG, "Web clip: too many redirects from $urlString")
        return null
    }

    /**
     * Detect Wikipedia URLs (including mobile) and fetch metadata via REST API.
     * Returns Pair(title, wikitextContent) or null if not Wikipedia or API fails.
     */
    private fun fetchWikipediaMetadata(urlString: String): Pair<String, String>? {
        // Match *.wikipedia.org/wiki/Title URLs (desktop and mobile)
        val wikiMatch = Regex(
            """https?://([a-z]{2,3}(?:\.[a-z]+)?)(\.m)?\.wikipedia\.org/wiki/([^#?]+)""",
            RegexOption.IGNORE_CASE
        ).find(urlString) ?: return null

        val lang = wikiMatch.groupValues[1]  // e.g. "en", "de", "simple"
        val titleEncoded = wikiMatch.groupValues[3]  // URL-encoded title

        return try {
            val apiUrl = URL("https://$lang.wikipedia.org/api/rest_v1/page/summary/$titleEncoded")
            val conn = apiUrl.openConnection() as HttpURLConnection
            conn.connectTimeout = 8000
            conn.readTimeout = 8000
            conn.setRequestProperty("Accept", "application/json")
            conn.setRequestProperty("User-Agent",
                "TiddlyDesktopRS/1.0 (Android; mailto:burningtreec@gmail.com)")

            if (conn.responseCode != 200) {
                conn.disconnect()
                return null
            }

            val json = JSONObject(conn.inputStream.bufferedReader().readText())
            conn.disconnect()

            val title = json.optString("displaytitle", "")
                .replace(Regex("<[^>]+>"), "")  // Strip HTML formatting
                .ifBlank { json.optString("title", "").replace("_", " ") }
            val tagline = json.optString("description", "")  // Short tagline
            val extract = json.optString("extract", "")      // First paragraph
            val imageUrl = json.optJSONObject("thumbnail")?.optString("source", "")
                ?: json.optJSONObject("originalimage")?.optString("source", "")
                ?: ""

            if (title.isBlank()) return null

            // Any selected text the user shared
            val selectedText = sharedText?.trim()

            val wikitext = buildString {
                if (imageUrl.isNotBlank() && imageUrl.startsWith("http")) {
                    appendLine("[img[$imageUrl]]")
                    appendLine()
                }
                // Use extract (first paragraph) as the blockquote, fall back to tagline
                val desc = extract.ifBlank { tagline }
                if (desc.isNotBlank()) {
                    appendLine("<<<")
                    appendLine(desc)
                    appendLine("<<<")
                    appendLine()
                }
                append("Source: [[$urlString]]")
                appendLine(" (Wikipedia)")
                if (tagline.isNotBlank() && extract.isNotBlank()) {
                    appendLine("//~$tagline//")
                }
                if (!selectedText.isNullOrBlank()) {
                    appendLine()
                    appendLine("---")
                    appendLine()
                    append(selectedText)
                }
            }.trim()

            Pair(title, wikitext)
        } catch (e: Exception) {
            Log.d(TAG, "Wikipedia API failed: ${e.message}, falling back to HTML scraping")
            null
        }
    }

    private fun fetchAndParseWebPage(urlString: String): Pair<String, String>? {
        // Try Wikipedia REST API first (cleaner data than HTML scraping)
        fetchWikipediaMetadata(urlString)?.let { return it }

        return try {
            val (finalUrl, rawBytes, httpCharset) = fetchUrl(urlString) ?: return null

            // Charset detection: always prefer UTF-8 if bytes are valid UTF-8.
            // Many servers/meta tags declare wrong charsets. Browsers default to UTF-8.
            val html = if (looksLikeUtf8(rawBytes)) {
                Log.d(TAG, "Web clip: using UTF-8 (valid UTF-8 bytes)")
                String(rawBytes, Charsets.UTF_8)
            } else {
                val charset = detectHtmlCharset(rawBytes, httpCharset)
                Log.d(TAG, "Web clip: using charset=$charset (not valid UTF-8)")
                String(rawBytes, java.nio.charset.Charset.forName(charset))
            }

            // Try JSON-LD first (more specific for sub-pages, forum posts, etc.)
            val jsonLd = extractJsonLd(html)

            // Extract title
            val title = extractHtmlTag(html, "og:title")
                ?: extractHtmlTitle(html)
                ?: finalUrl

            // Prefer JSON-LD data over OG tags (JSON-LD is often page-specific)
            var description = jsonLd["description"]
                ?: extractHtmlTag(html, "og:description")
                ?: extractHtmlTag(html, "twitter:description")
                ?: extractMetaDescription(html)
                ?: ""
            // Fall back to first substantial <p> tag if description is too short
            if (description.length < 50) {
                val firstPara = extractFirstParagraph(html)
                if (firstPara != null && firstPara.length > description.length) {
                    description = firstPara
                }
            }
            val ogImage = jsonLd["image"]
                ?: extractHtmlTag(html, "og:image")
                ?: extractHtmlTag(html, "twitter:image")
            val siteName = extractHtmlTag(html, "og:site_name") ?: ""
            val author = jsonLd["author"]
                ?: extractHtmlTag(html, "author")
                ?: extractHtmlTag(html, "article:author")
                ?: ""
            val publishedTime = jsonLd["datePublished"]
                ?: extractHtmlTag(html, "article:published_time")
                ?: extractHtmlTag(html, "datePublished")
                ?: ""

            // Use the final URL after redirects for the Source link
            val sourceUrl = finalUrl

            // Any selected text the user shared (text beyond the URL)
            // sharedText has URL already stripped in handleTextIntent
            val selectedText = sharedText?.trim()

            // Build meta preview wikitext (no body content extraction)
            val wikitext = buildString {
                if (ogImage != null && ogImage.startsWith("http")) {
                    appendLine("[img[${ogImage}]]")
                    appendLine()
                }
                if (description.isNotBlank()) {
                    appendLine("<<<")
                    appendLine(description)
                    appendLine("<<<")
                    appendLine()
                }
                append("Source: [[${sourceUrl}]]")
                if (siteName.isNotBlank()) append(" (${siteName})")
                appendLine()
                if (author.isNotBlank()) appendLine("Author: $author")
                if (publishedTime.isNotBlank()) {
                    val dateOnly = publishedTime.substringBefore("T")
                    appendLine("Published: $dateOnly")
                }
                if (!selectedText.isNullOrBlank()) {
                    appendLine()
                    appendLine("---")
                    appendLine()
                    append(selectedText)
                }
            }.trim()

            // Also update the detected URL for the capture file's source_url
            detectedUrl = sourceUrl

            Pair(title, wikitext)
        } catch (e: Exception) {
            Log.e(TAG, "Web clip failed: ${e.message}")
            null
        }
    }

    /**
     * Detect the actual charset of HTML content by checking (in priority order):
     * 1. BOM (Byte Order Mark)
     * 2. HTML <meta charset="..."> tag
     * 3. HTML <meta http-equiv="Content-Type" content="...;charset=..."> tag
     * 4. HTTP Content-Type header charset
     * 5. Default to UTF-8
     */
    private fun detectHtmlCharset(rawBytes: ByteArray, httpCharset: String?): String {
        // Check for UTF-8 BOM
        if (rawBytes.size >= 3 &&
            rawBytes[0] == 0xEF.toByte() && rawBytes[1] == 0xBB.toByte() && rawBytes[2] == 0xBF.toByte()) {
            return "UTF-8"
        }

        // Scan the first 4KB as ASCII/Latin-1 to find meta charset declarations
        // (charset declarations must appear early in the document)
        val preview = String(rawBytes, 0, minOf(rawBytes.size, 4096), Charsets.ISO_8859_1)

        // <meta charset="UTF-8"> (charset can be anywhere in the tag)
        val metaCharset = Regex(
            """<meta\s[^>]*charset\s*=\s*["']?([^"'\s;>]+)""",
            RegexOption.IGNORE_CASE
        ).find(preview)
        if (metaCharset != null) {
            val cs = metaCharset.groupValues[1].trim()
            return try { java.nio.charset.Charset.forName(cs); cs } catch (_: Exception) { "UTF-8" }
        }

        // <meta http-equiv="Content-Type" content="text/html; charset=UTF-8">
        // Search for charset= inside any meta content attribute
        val metaHttpEquiv = Regex(
            """<meta\s[^>]*content\s*=\s*["'][^"']*charset=([^"'\s;]+)""",
            RegexOption.IGNORE_CASE
        ).find(preview)
        if (metaHttpEquiv != null) {
            val cs = metaHttpEquiv.groupValues[1].trim()
            return try { java.nio.charset.Charset.forName(cs); cs } catch (_: Exception) { "UTF-8" }
        }

        // Fall back to HTTP header charset, but validate against actual content.
        // Many servers declare ISO-8859-1 or Windows-1252 while serving UTF-8 content.
        if (httpCharset != null) {
            val normalized = httpCharset.uppercase()
            if (normalized != "UTF-8" && normalized != "UTF8") {
                // If HTTP says non-UTF-8, check if content is actually valid UTF-8
                if (looksLikeUtf8(rawBytes)) {
                    return "UTF-8"
                }
            }
            return try { java.nio.charset.Charset.forName(httpCharset); httpCharset } catch (_: Exception) { "UTF-8" }
        }

        return "UTF-8"
    }

    /**
     * Quick check if raw bytes look like valid UTF-8 by scanning for multi-byte sequences.
     * Returns true if we find valid multi-byte UTF-8 sequences and no invalid ones.
     * Scans up to 8KB for efficiency.
     */
    private fun looksLikeUtf8(rawBytes: ByteArray): Boolean {
        val limit = minOf(rawBytes.size, 8192)
        var i = 0
        var multiByteCount = 0
        while (i < limit) {
            val b = rawBytes[i].toInt() and 0xFF
            when {
                b <= 0x7F -> i++ // ASCII
                b in 0xC2..0xDF -> { // 2-byte
                    if (i + 1 >= limit) break
                    val b2 = rawBytes[i + 1].toInt() and 0xFF
                    if (b2 !in 0x80..0xBF) return false
                    multiByteCount++
                    i += 2
                }
                b in 0xE0..0xEF -> { // 3-byte
                    if (i + 2 >= limit) break
                    val b2 = rawBytes[i + 1].toInt() and 0xFF
                    val b3 = rawBytes[i + 2].toInt() and 0xFF
                    if (b2 !in 0x80..0xBF || b3 !in 0x80..0xBF) return false
                    multiByteCount++
                    i += 3
                }
                b in 0xF0..0xF4 -> { // 4-byte
                    if (i + 3 >= limit) break
                    val b2 = rawBytes[i + 1].toInt() and 0xFF
                    val b3 = rawBytes[i + 2].toInt() and 0xFF
                    val b4 = rawBytes[i + 3].toInt() and 0xFF
                    if (b2 !in 0x80..0xBF || b3 !in 0x80..0xBF || b4 !in 0x80..0xBF) return false
                    multiByteCount++
                    i += 4
                }
                else -> return false // Invalid UTF-8 leading byte
            }
        }
        // Only claim UTF-8 if we actually found multi-byte sequences
        return multiByteCount > 0
    }

    private fun extractHtmlTitle(html: String): String? {
        val match = Regex("<title[^>]*>(.*?)</title>", RegexOption.DOT_MATCHES_ALL).find(html)
        return match?.groupValues?.get(1)?.trim()?.let { decodeHtmlEntities(it) }
    }

    private fun extractHtmlTag(html: String, property: String): String? {
        val p1 = "<meta\\s+(?:[^>]*?)(?:property|name)=[\"']${property}[\"'][^>]*?content=[\"'](.*?)[\"']"
        val match = Regex(p1, RegexOption.IGNORE_CASE).find(html)
        if (match != null) return decodeHtmlEntities(match.groupValues[1].trim())

        // Try reversed attribute order
        val p2 = "<meta\\s+(?:[^>]*?)content=[\"'](.*?)[\"'][^>]*?(?:property|name)=[\"']${property}[\"']"
        val match2 = Regex(p2, RegexOption.IGNORE_CASE).find(html)
        return match2?.groupValues?.get(1)?.trim()?.let { decodeHtmlEntities(it) }
    }

    private fun extractMetaDescription(html: String): String? {
        return extractHtmlTag(html, "description")
    }

    /**
     * Extract the first substantial <p> tag from HTML body as a fallback description.
     * Strips boilerplate sections (nav, header, footer, aside, script, style) first.
     * Returns cleaned text or null if no substantial paragraph found.
     */
    private fun extractFirstParagraph(html: String, minLength: Int = 50): String? {
        // Strip non-content blocks to avoid cookie banners, nav, etc.
        val stripped = html
            .replace(Regex("<script[^>]*>.*?</script>", setOf(RegexOption.DOT_MATCHES_ALL, RegexOption.IGNORE_CASE)), "")
            .replace(Regex("<style[^>]*>.*?</style>", setOf(RegexOption.DOT_MATCHES_ALL, RegexOption.IGNORE_CASE)), "")
            .replace(Regex("<nav[^>]*>.*?</nav>", setOf(RegexOption.DOT_MATCHES_ALL, RegexOption.IGNORE_CASE)), "")
            .replace(Regex("<header[^>]*>.*?</header>", setOf(RegexOption.DOT_MATCHES_ALL, RegexOption.IGNORE_CASE)), "")
            .replace(Regex("<footer[^>]*>.*?</footer>", setOf(RegexOption.DOT_MATCHES_ALL, RegexOption.IGNORE_CASE)), "")
            .replace(Regex("<aside[^>]*>.*?</aside>", setOf(RegexOption.DOT_MATCHES_ALL, RegexOption.IGNORE_CASE)), "")

        // Find all <p> tags and return the first one with enough content
        val paragraphs = Regex(
            """<p[^>]*>(.*?)</p>""",
            setOf(RegexOption.DOT_MATCHES_ALL, RegexOption.IGNORE_CASE)
        ).findAll(stripped)

        for (p in paragraphs) {
            val text = p.groupValues[1]
                .replace(Regex("<[^>]+>"), "")  // Strip inner HTML tags
                .replace(Regex("\\s+"), " ")
                .trim()
            // Skip short fragments, cookie notices, login prompts
            if (text.length < minLength) continue
            if (text.contains("cookie", ignoreCase = true) && text.contains("accept", ignoreCase = true)) continue
            if (text.contains("sign in", ignoreCase = true) || text.contains("log in", ignoreCase = true)) continue
            return decodeHtmlEntities(text).take(500)
        }
        return null
    }

    private fun decodeHtmlEntities(text: String): String {
        return text
            .replace("&amp;", "&")
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&#39;", "'")
            .replace("&apos;", "'")
            .replace("&nbsp;", " ")
            .replace(Regex("""&#(\d+);""")) { m ->
                val code = m.groupValues[1].toIntOrNull()
                if (code != null) String(Character.toChars(code)) else m.value
            }
            .replace(Regex("""&#x([0-9a-fA-F]+);""")) { m ->
                val code = m.groupValues[1].toIntOrNull(16)
                if (code != null) String(Character.toChars(code)) else m.value
            }
    }

    /**
     * Extract the documentTopTreeUri from a wiki path JSON string.
     * Wiki path format: {"uri":"content://...","documentTopTreeUri":"content://..."}
     */
    private fun extractTreeUri(wikiPath: String): Uri? {
        return try {
            val json = JSONObject(wikiPath)
            val treeUriStr = json.optString("documentTopTreeUri", "")
            if (treeUriStr.isNotEmpty() && treeUriStr != "null") Uri.parse(treeUriStr) else null
        } catch (e: Exception) {
            null
        }
    }

    /**
     * Copy the shared file to the wiki's attachments folder as an external attachment.
     * Returns the relative path (e.g. "./attachments/photo.jpg") or null on failure.
     */
    private fun getFileSize(uri: Uri): Long {
        return try {
            contentResolver.openAssetFileDescriptor(uri, "r")?.use { it.length } ?: -1L
        } catch (e: Exception) { -1L }
    }

    private fun filesMatchBySize(uri1: Uri, uri2: Uri): Boolean {
        val size1 = getFileSize(uri1)
        val size2 = getFileSize(uri2)
        if (size1 > 0 && size1 == size2) return true
        if (size1 < 0 && size2 < 0) return true  // Both unknown, assume same
        return false
    }

    private fun copyToAttachmentsFolder(sourceUri: Uri, treeUri: Uri, suggestedName: String, mimeType: String): String? {
        return try {
            val parentDoc = DocumentFile.fromTreeUri(this, treeUri) ?: return null

            // Find or create attachments directory
            var attachmentsDir = parentDoc.findFile("attachments")
            if (attachmentsDir == null || !attachmentsDir.isDirectory) {
                attachmentsDir = parentDoc.createDirectory("attachments") ?: return null
            }

            // Sanitize filename
            val safeName = suggestedName.replace("/", "_").replace("\\", "_")

            // Check for existing file — reuse if same size (deduplication)
            var finalName = safeName
            var existingFile = attachmentsDir.findFile(safeName)
            if (existingFile != null) {
                if (filesMatchBySize(sourceUri, existingFile.uri)) {
                    Log.d(TAG, "Attachment already exists with matching size: ./attachments/$safeName")
                    return "./attachments/$safeName"
                }
                // Different size, find unique name
                val baseName = safeName.substringBeforeLast(".")
                val ext = safeName.substringAfterLast(".", "")
                var counter = 1
                do {
                    finalName = if (ext.isNotEmpty()) "${baseName}_${counter}.${ext}" else "${baseName}_${counter}"
                    existingFile = attachmentsDir.findFile(finalName)
                    if (existingFile != null && filesMatchBySize(sourceUri, existingFile.uri)) {
                        Log.d(TAG, "Attachment already exists with matching size: ./attachments/$finalName")
                        return "./attachments/$finalName"
                    }
                    counter++
                } while (existingFile != null && counter < 1000)
            }

            // Create file and copy content
            val targetFile = attachmentsDir.createFile(mimeType, finalName) ?: return null
            contentResolver.openInputStream(sourceUri)?.use { input ->
                contentResolver.openOutputStream(targetFile.uri)?.use { output ->
                    input.copyTo(output)
                }
            }

            Log.d(TAG, "Copied attachment to: ./attachments/$finalName")
            "./attachments/$finalName"
        } catch (e: Exception) {
            Log.e(TAG, "Failed to copy to attachments: ${e.message}")
            null
        }
    }

    private fun saveCapture() {
        val selectedIndex = wikiSpinner?.selectedItemPosition ?: 0
        if (selectedIndex < 0 || selectedIndex >= wikiList.size) {
            Toast.makeText(this, getString(R.string.capture_select_wiki), Toast.LENGTH_SHORT).show()
            return
        }

        val wiki = wikiList[selectedIndex]
        // Remember selected wiki for next time
        getSharedPreferences("capture_prefs", MODE_PRIVATE).edit()
            .putString("last_wiki_path", wiki.path).apply()
        val title = titleEdit?.text?.toString()?.let { sanitizeTitle(it) }?.ifBlank { null } ?: getString(R.string.capture_untitled)
        val tags = tagsEdit?.text?.toString()?.trim() ?: ""
        val now = System.currentTimeMillis()

        // Handle SEND_MULTIPLE — save each item as a separate capture (no title prefix)
        if (multipleUris != null) {
            saveMultipleCaptures(wiki, "", tags, now)
            return
        }

        val captureJson = JSONObject().apply {
            put("target_wiki_path", wiki.path)
            put("target_wiki_title", wiki.title)
            put("title", title.ifEmpty { getString(R.string.capture_untitled) })
            put("tags", tags)
            put("created", now)
        }

        if (sharedImageUri != null) {
            // Determine MIME type and filename
            val mimeType = contentResolver.getType(sharedImageUri!!) ?: "image/jpeg"
            val ext = MimeTypeMap.getSingleton().getExtensionFromMimeType(mimeType) ?: "jpg"
            val filename = getDisplayName(sharedImageUri!!) ?: "capture_${now}.${ext}"

            // Store caption from EXTRA_TEXT (e.g. WhatsApp image captions)
            if (!sharedText.isNullOrBlank()) {
                captureJson.put("caption", sharedText!!.trim())
            }

            // Try external attachment: copy file to wiki's attachments folder
            val treeUri = extractTreeUri(wiki.path)
            if (treeUri != null && wiki.externalAttachments) {
                val relativePath = copyToAttachmentsFolder(sharedImageUri!!, treeUri, filename, mimeType)
                if (relativePath != null) {
                    captureJson.put("type", mimeType)
                    captureJson.put("_canonical_uri", relativePath)
                    captureJson.put("text", "")
                } else {
                    // Fallback to base64 if copy fails
                    embedImageAsBase64(captureJson)
                }
            } else {
                // External attachments disabled or no folder access, embed as base64
                embedImageAsBase64(captureJson)
            }
        } else if (sharedFileUri != null) {
            // Video/audio/PDF file — requires external attachments (too large for base64)
            val mimeType = contentResolver.getType(sharedFileUri!!) ?: intent.type ?: "application/octet-stream"
            val ext = MimeTypeMap.getSingleton().getExtensionFromMimeType(mimeType) ?: "bin"
            val filename = getDisplayName(sharedFileUri!!) ?: "capture_${now}.${ext}"

            val treeUri = extractTreeUri(wiki.path)
            if (treeUri != null && wiki.externalAttachments) {
                val relativePath = copyToAttachmentsFolder(sharedFileUri!!, treeUri, filename, mimeType)
                if (relativePath != null) {
                    captureJson.put("type", mimeType)
                    captureJson.put("_canonical_uri", relativePath)
                    captureJson.put("text", "")
                } else {
                    Toast.makeText(this, getString(R.string.capture_failed_copy), Toast.LENGTH_LONG).show()
                    return
                }
            } else {
                Toast.makeText(this, getString(R.string.capture_no_attachments), Toast.LENGTH_LONG).show()
                return
            }
        } else if (importFileUri != null) {
            // Native TiddlyWiki import — copy file and let TW handle parsing
            val importFilename = "import_${now}.dat"
            val capturesDir = File(filesDir, "captures")
            if (!capturesDir.exists()) capturesDir.mkdirs()
            val importFile = File(capturesDir, importFilename)

            try {
                contentResolver.openInputStream(importFileUri!!)?.use { input ->
                    importFile.outputStream().use { output ->
                        input.copyTo(output)
                    }
                }
            } catch (e: Exception) {
                Log.e(TAG, "Failed to copy import file: ${e.message}")
                Toast.makeText(this, getString(R.string.capture_failed_read), Toast.LENGTH_LONG).show()
                return
            }

            captureJson.put("import_file", importFilename)
            captureJson.put("file_type", importFileMimeType)
            captureJson.put("text", "")
        } else if (sharedTextFileUri != null) {
            // Text-based file shared from file manager (.txt, .md, .svg) —
            // keep external when attachments enabled, otherwise embed
            val textUri = sharedTextFileUri!!
            val ext = MimeTypeMap.getSingleton().getExtensionFromMimeType(captureType) ?: "txt"
            val filename = getDisplayName(textUri) ?: "capture_${now}.${ext}"
            val treeUri = extractTreeUri(wiki.path)
            if (treeUri != null && wiki.externalAttachments) {
                val relativePath = copyToAttachmentsFolder(textUri, treeUri, filename, captureType)
                if (relativePath != null) {
                    captureJson.put("type", captureType)
                    captureJson.put("_canonical_uri", relativePath)
                    captureJson.put("text", "")
                } else {
                    // Fallback to embedding text if copy fails
                    captureJson.put("type", captureType)
                    captureJson.put("text", sharedText ?: "")
                }
            } else {
                // External attachments disabled — embed text directly
                captureJson.put("type", captureType)
                captureJson.put("text", sharedText ?: "")
            }
        } else {
            // Text capture (plain text, markdown, SVG, contacts, calendar)
            captureJson.put("type", captureType)

            val text = when {
                clippedContent != null -> clippedContent!!
                detectedUrl != null -> {
                    // Sanitize title: strip line breaks, collapse whitespace
                    val urlTitle = (sharedSubject ?: detectedUrl!!)
                        .replace("\n", " ").replace("\r", " ")
                        .replace(Regex("\\s+"), " ").trim()
                    val link = "[[$urlTitle|$detectedUrl]]"
                    // sharedText has URL already stripped in handleTextIntent
                    val extraText = sharedText?.trim()
                    if (!extraText.isNullOrBlank()) {
                        "$extraText\n\n$link"
                    } else {
                        link
                    }
                }
                else -> contentEdit?.text?.toString()?.trim() ?: sharedText ?: ""
            }
            captureJson.put("text", text)

            if (detectedUrl != null) {
                captureJson.put("source_url", detectedUrl)
            }
        }

        writeCaptureAndFinish(captureJson, wiki)
    }

    /**
     * Save multiple items from SEND_MULTIPLE intent, one capture file per item.
     */
    private fun saveMultipleCaptures(wiki: WikiEntry, titlePrefix: String, tags: String, now: Long) {
        val uris = multipleUris ?: return
        val treeUri = extractTreeUri(wiki.path)
        val capturesDir = File(filesDir, "captures")
        if (!capturesDir.exists()) capturesDir.mkdirs()

        var savedCount = 0
        for ((index, uri) in uris.withIndex()) {
            val mimeType = contentResolver.getType(uri) ?: "application/octet-stream"
            val ext = MimeTypeMap.getSingleton().getExtensionFromMimeType(mimeType) ?: "bin"
            val displayName = getDisplayName(uri) ?: "item_${index + 1}.${ext}"
            val itemTitle = if (titlePrefix.isNotBlank()) {
                if (uris.size > 1) "$titlePrefix ${index + 1}" else titlePrefix
            } else {
                displayName.substringBeforeLast(".")
            }

            val captureJson = JSONObject().apply {
                put("target_wiki_path", wiki.path)
                put("target_wiki_title", wiki.title)
                put("title", itemTitle)
                put("tags", tags)
                put("created", now + index) // offset to ensure unique timestamps
            }

            if (treeUri != null && wiki.externalAttachments) {
                // Folder wiki with external attachments enabled — copy as external attachment
                val relativePath = copyToAttachmentsFolder(uri, treeUri, displayName, mimeType)
                if (relativePath != null) {
                    captureJson.put("type", mimeType)
                    captureJson.put("_canonical_uri", relativePath)
                    captureJson.put("text", "")
                } else {
                    // Try base64 fallback for images
                    if (mimeType.startsWith("image/") && !mimeType.contains("svg")) {
                        embedUriAsBase64(captureJson, uri)
                    } else {
                        continue // skip this item
                    }
                }
            } else if (mimeType.startsWith("image/") && !mimeType.contains("svg")) {
                // Single-file wiki — embed images as base64
                embedUriAsBase64(captureJson, uri)
            } else {
                // Can't embed large files in single-file wiki
                continue
            }

            val random = (('a'..'z') + ('0'..'9')).shuffled().take(4).joinToString("")
            val captureFile = File(capturesDir, "capture_${now + index}_${random}.json")
            try {
                captureFile.writeText(captureJson.toString(2))
                savedCount++
            } catch (e: Exception) {
                Log.e(TAG, "Failed to save capture ${index + 1}: ${e.message}")
            }
        }

        if (savedCount == 0) {
            Toast.makeText(this, getString(R.string.capture_no_attachments), Toast.LENGTH_LONG).show()
            return
        }

        AlertDialog.Builder(this)
            .setTitle(getString(R.string.capture_saved_title))
            .setMessage(getString(R.string.capture_saved_message, wiki.title))
            .setPositiveButton(getString(R.string.btn_ok)) { _, _ -> finish() }
            .setNeutralButton(getString(R.string.btn_open_wiki)) { _, _ ->
                launchWikiActivity(wiki.path, wiki.title, wiki.isFolder)
                finish()
            }
            .setCancelable(false)
            .show()
    }

    private fun embedUriAsBase64(captureJson: JSONObject, uri: Uri) {
        try {
            contentResolver.openInputStream(uri)?.use { stream ->
                val bitmap = BitmapFactory.decodeStream(stream)
                if (bitmap != null) {
                    captureJson.put("type", "image/jpeg")
                    val base64 = bitmapToBase64(bitmap, 85)
                    captureJson.put("text", if (base64.length > 5 * 1024 * 1024) bitmapToBase64(bitmap, 60) else base64)
                }
            }
        } catch (e: Exception) {
            Log.e(TAG, "Failed to embed image as base64: ${e.message}")
        }
    }

    private fun writeCaptureAndFinish(captureJson: JSONObject, wiki: WikiEntry) {
        val capturesDir = File(filesDir, "captures")
        if (!capturesDir.exists()) capturesDir.mkdirs()

        val now = System.currentTimeMillis()
        val random = (('a'..'z') + ('0'..'9')).shuffled().take(4).joinToString("")
        val captureFile = File(capturesDir, "capture_${now}_${random}.json")

        try {
            captureFile.writeText(captureJson.toString(2))
            Log.d(TAG, "Capture saved: ${captureFile.name}")
        } catch (e: Exception) {
            Log.e(TAG, "Failed to save capture: ${e.message}")
            Toast.makeText(this, getString(R.string.capture_failed_save), Toast.LENGTH_LONG).show()
            return
        }

        // Show success dialog
        AlertDialog.Builder(this)
            .setTitle(getString(R.string.capture_saved_title))
            .setMessage(getString(R.string.capture_saved_message, wiki.title))
            .setPositiveButton(getString(R.string.btn_ok)) { _, _ -> finish() }
            .setNeutralButton(getString(R.string.btn_open_wiki)) { _, _ ->
                launchWikiActivity(wiki.path, wiki.title, wiki.isFolder)
                finish()
            }
            .setCancelable(false)
            .show()
    }

    private fun embedImageAsBase64(captureJson: JSONObject) {
        if (imageBitmap == null) return
        captureJson.put("type", "image/jpeg")
        val base64 = bitmapToBase64(imageBitmap!!, 85)
        if (base64.length > 5 * 1024 * 1024) {
            captureJson.put("text", bitmapToBase64(imageBitmap!!, 60))
        } else {
            captureJson.put("text", base64)
        }
    }

    private fun bitmapToBase64(bitmap: Bitmap, quality: Int): String {
        val baos = ByteArrayOutputStream()
        bitmap.compress(Bitmap.CompressFormat.JPEG, quality, baos)
        return Base64.encodeToString(baos.toByteArray(), Base64.NO_WRAP)
    }

    private fun getDisplayName(uri: Uri): String? {
        try {
            contentResolver.query(uri, null, null, null, null)?.use { cursor ->
                if (cursor.moveToFirst()) {
                    val nameIndex = cursor.getColumnIndex(android.provider.OpenableColumns.DISPLAY_NAME)
                    if (nameIndex >= 0) {
                        return cursor.getString(nameIndex)
                    }
                }
            }
        } catch (e: Exception) {
            Log.w(TAG, "Could not get display name: ${e.message}")
        }
        return null
    }

    private fun launchWikiActivity(wikiPath: String, wikiTitle: String, isFolder: Boolean) {
        // Try to bring existing wiki to foreground first
        if (WikiActivity.bringWikiToFront(this, wikiPath)) {
            Log.d(TAG, "Wiki already open, brought to foreground")
            return
        }

        if (isFolder) {
            // Folder wikis need Node.js server started by Rust — launch the main app
            // with extras so handleWidgetIntent() writes pending_widget_wiki.json,
            // which the landing page reads and auto-opens the folder wiki.
            Log.d(TAG, "Folder wiki not yet open, launching main app with open_wiki extras")
            val mainIntent = packageManager.getLaunchIntentForPackage(packageName)
            if (mainIntent != null) {
                mainIntent.putExtra("open_wiki_path", wikiPath)
                mainIntent.putExtra("open_wiki_title", wikiTitle)
                mainIntent.putExtra("open_wiki_is_folder", true)
                mainIntent.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TOP)
                startActivity(mainIntent)
            }
            return
        }

        val wikiIntent = Intent(this, WikiActivity::class.java).apply {
            putExtra(WikiActivity.EXTRA_WIKI_PATH, wikiPath)
            putExtra(WikiActivity.EXTRA_WIKI_TITLE, wikiTitle)
            putExtra(WikiActivity.EXTRA_IS_FOLDER, isFolder)
            putExtra(WikiActivity.EXTRA_BACKUPS_ENABLED, true)
            putExtra(WikiActivity.EXTRA_BACKUP_COUNT, 20)
            addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
        }
        startActivity(wikiIntent)
    }

    /**
     * Clean up stale capture files older than 24 hours.
     * Also removes orphaned import data files (.dat) whose capture JSON no longer exists.
     */
    private fun cleanupStaleCaptureFiles() {
        try {
            val capturesDir = File(filesDir, "captures")
            if (!capturesDir.exists() || !capturesDir.isDirectory) return
            val files = capturesDir.listFiles() ?: return
            val now = System.currentTimeMillis()
            val maxAge = 24 * 60 * 60 * 1000L  // 24 hours

            // Collect all referenced import filenames from still-valid capture JSONs
            val referencedImports = mutableSetOf<String>()
            var deletedCount = 0

            for (f in files) {
                if (f.name.startsWith("capture_") && f.name.endsWith(".json")) {
                    try {
                        val json = JSONObject(f.readText())
                        val created = json.optLong("created", 0)
                        if (created > 0 && now - created > maxAge) {
                            // Also delete any referenced import file
                            val importFile = json.optString("import_file", "")
                            if (importFile.isNotEmpty()) {
                                File(capturesDir, importFile).delete()
                            }
                            f.delete()
                            deletedCount++
                        } else {
                            // Still valid — track its import file reference
                            val importFile = json.optString("import_file", "")
                            if (importFile.isNotEmpty()) referencedImports.add(importFile)
                        }
                    } catch (e: Exception) {
                        // Malformed JSON — delete if old (by file modification time)
                        if (now - f.lastModified() > maxAge) {
                            f.delete()
                            deletedCount++
                        }
                    }
                }
            }

            // Clean orphaned .dat import files not referenced by any capture JSON
            for (f in files) {
                if (f.name.startsWith("import_") && f.name.endsWith(".dat")) {
                    if (f.name !in referencedImports) {
                        f.delete()
                        deletedCount++
                    }
                }
            }

            if (deletedCount > 0) {
                Log.d(TAG, "Cleaned up $deletedCount stale capture file(s)")
            }
        } catch (e: Exception) {
            Log.w(TAG, "Capture cleanup failed: ${e.message}")
        }
    }
}
