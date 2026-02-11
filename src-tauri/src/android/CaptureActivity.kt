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
    }

    private var sharedText: String? = null
    private var sharedSubject: String? = null
    private var sharedImageUri: Uri? = null
    private var detectedUrl: String? = null
    private var imageBitmap: Bitmap? = null
    private var sharedFileUri: Uri? = null
    private var fileThumbnail: Bitmap? = null
    private var importFileUri: Uri? = null
    private var importFileName: String? = null
    private var importFileMimeType: String? = null
    private var wikiList: List<WikiEntry> = emptyList()

    // UI references
    private var titleEdit: EditText? = null
    private var tagsEdit: EditText? = null
    private var wikiSpinner: Spinner? = null
    // clipButton removed — web clip is now automatic for URL shares
    private var clipProgress: ProgressBar? = null
    private var clippedContent: String? = null

    data class WikiEntry(val path: String, val title: String, val isFolder: Boolean)

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        if (intent.action != Intent.ACTION_SEND) {
            Log.w(TAG, "Unsupported action: ${intent.action}")
            finish()
            return
        }

        wikiList = loadRecentWikis()

        when {
            intent.type == "text/plain" -> handleTextIntent()
            intent.type?.startsWith("image/") == true -> handleImageIntent()
            intent.type?.startsWith("video/") == true ||
            intent.type?.startsWith("audio/") == true -> handleFileIntent()
            intent.type == "text/html" ||
            intent.type == "application/xhtml+xml" ||
            intent.type == "application/json" ||
            intent.type == "text/csv" -> handleNativeImportIntent()
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
        sharedText = intent.getStringExtra(Intent.EXTRA_TEXT)
        sharedSubject = intent.getStringExtra(Intent.EXTRA_SUBJECT)

        if (sharedText.isNullOrBlank()) {
            Toast.makeText(this, getString(R.string.capture_no_text), Toast.LENGTH_SHORT).show()
            finish()
            return
        }

        // Detect URL in shared text
        val matcher = URL_PATTERN.matcher(sharedText!!)
        if (matcher.find()) {
            val rawUrl = matcher.group()
            // Strip Chrome's text fragment (#:~:text=...) from URL for clean links
            detectedUrl = rawUrl.replace(Regex("#:~:.*$"), "")
            // Remove the raw URL (with fragment) from sharedText to extract selected text
            sharedText = sharedText!!.replace(rawUrl, "").trim().ifBlank { null }
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

        sharedSubject = intent.getStringExtra(Intent.EXTRA_SUBJECT)
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

        sharedSubject = intent.getStringExtra(Intent.EXTRA_SUBJECT)
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
            else -> contentResolver.getType(streamUri) ?: intent.type ?: "text/plain"
        }

        sharedSubject = importFileName
    }

    private fun loadRecentWikis(): List<WikiEntry> {
        val wikis = mutableListOf<WikiEntry>()

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
                    if (path.isNotEmpty() && title.isNotEmpty()) {
                        wikis.add(WikiEntry(path, title, isFolder))
                    }
                }
            } catch (e: Exception) {
                Log.w(TAG, "Failed to read Kotlin recent_wikis.json: ${e.message}")
            }
        }

        if (wikis.isEmpty()) {
            // Fall back to Rust-side recent_wikis.json
            val rustFile = File(filesDir.parentFile, "recent_wikis.json")
            if (rustFile.exists()) {
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
                    Log.w(TAG, "Failed to read Rust recent_wikis.json: ${e.message}")
                }
            }
        }

        return wikis
    }

    private fun dp(value: Int): Int {
        return TypedValue.applyDimension(
            TypedValue.COMPLEX_UNIT_DIP, value.toFloat(), resources.displayMetrics
        ).toInt()
    }

    // Material Design color palette
    private val colorPrimary = 0xFF6750A4.toInt()       // M3 primary
    private val colorOnPrimary = 0xFFFFFFFF.toInt()
    private val colorPrimaryContainer = 0xFFEADDFF.toInt()
    private val colorOnPrimaryContainer = 0xFF21005D.toInt()
    private val colorSurface = 0xFFFFFBFE.toInt()
    private val colorSurfaceVariant = 0xFFE7E0EC.toInt()
    private val colorOnSurface = 0xFF1C1B1F.toInt()
    private val colorOnSurfaceVariant = 0xFF49454F.toInt()
    private val colorOutline = 0xFF79747E.toInt()
    private val colorOutlineVariant = 0xFFCAC4D0.toInt()
    private val colorError = 0xFFB3261E.toInt()
    private val colorScrim = 0x52000000.toInt()          // 32% black

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

        // Title field with outlined style
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
                setText(getString(R.string.capture_import_format, importFileName))
                isEnabled = false
            } else {
                val defaultTitle = sharedSubject
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
                    if (titleEdit?.text.isNullOrBlank() || titleEdit?.text.toString() == detectedUrl) {
                        titleEdit?.setText(result.first)
                    }
                }
                // On failure, save will fall back to [[title|url]] bookmark
            }
        }.start()
    }

    /**
     * Extract structured data from JSON-LD (<script type="application/ld+json">).
     * Returns a map with keys: description, author, datePublished, image.
     * Prefers the LAST JSON-LD block (often the most specific on multi-content pages).
     */
    private fun extractJsonLd(html: String): Map<String, String> {
        val result = mutableMapOf<String, String>()
        val blocks = Regex(
            """<script\s+type\s*=\s*["']application/ld\+json["'][^>]*>(.*?)</script>""",
            setOf(RegexOption.DOT_MATCHES_ALL, RegexOption.IGNORE_CASE)
        ).findAll(html)

        for (match in blocks) {
            try {
                val jsonStr = match.groupValues[1].trim()
                val json = org.json.JSONObject(jsonStr)

                // Description: text > articleBody > description
                val desc = json.optString("text", "").ifBlank {
                    json.optString("articleBody", "").ifBlank {
                        json.optString("description", "")
                    }
                }
                if (desc.isNotBlank()) {
                    // Strip HTML tags and limit length
                    val clean = Regex("<[^>]+>").replace(desc, "")
                        .replace(Regex("\\s+"), " ").trim().take(500)
                    if (clean.isNotBlank()) result["description"] = clean
                }

                // Author
                val authorObj = json.optJSONObject("author")
                val authorName = authorObj?.optString("name", "")
                    ?: json.optString("author", "")
                if (authorName.isNotBlank()) result["author"] = authorName

                // Date
                val date = json.optString("datePublished", "").ifBlank {
                    json.optString("dateCreated", "")
                }
                if (date.isNotBlank()) result["datePublished"] = date

                // Image
                val imageObj = json.optJSONObject("image")
                val imageUrl = imageObj?.optString("url", "")
                    ?: json.optString("image", "")
                if (imageUrl.isNotBlank() && imageUrl.startsWith("http")) result["image"] = imageUrl

            } catch (e: Exception) {
                // Skip malformed JSON-LD blocks
            }
        }
        return result
    }

    /**
     * Fetch a web page's meta preview and build a wikitext card.
     * Uses JSON-LD structured data when available (more specific for sub-pages/posts),
     * falls back to OG/meta tags. Does NOT extract body content.
     * Returns Pair(title, wikitextContent) or null on failure.
     */
    private fun fetchAndParseWebPage(urlString: String): Pair<String, String>? {
        return try {
            val url = URL(urlString)
            val conn = url.openConnection() as HttpURLConnection
            conn.connectTimeout = 10000
            conn.readTimeout = 10000
            conn.instanceFollowRedirects = true
            conn.setRequestProperty("User-Agent", "Mozilla/5.0 (Android; TiddlyDesktop) AppleWebKit/537.36")

            val responseCode = conn.responseCode
            if (responseCode != 200) {
                Log.w(TAG, "Web clip failed: HTTP $responseCode")
                conn.disconnect()
                return null
            }

            // Determine charset
            val contentType = conn.contentType ?: ""
            val charset = if (contentType.contains("charset=", ignoreCase = true)) {
                contentType.substringAfter("charset=").substringBefore(";").trim()
            } else {
                "UTF-8"
            }

            val html = conn.inputStream.bufferedReader(java.nio.charset.Charset.forName(charset)).readText()
            conn.disconnect()

            // Try JSON-LD first (more specific for sub-pages, forum posts, etc.)
            val jsonLd = extractJsonLd(html)

            // Extract title
            val title = extractHtmlTag(html, "og:title")
                ?: extractHtmlTitle(html)
                ?: urlString

            // Prefer JSON-LD data over OG tags (JSON-LD is often page-specific)
            val description = jsonLd["description"]
                ?: extractHtmlTag(html, "og:description")
                ?: extractHtmlTag(html, "twitter:description")
                ?: extractMetaDescription(html)
                ?: ""
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
                append("Source: [[${urlString}]]")
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

            Pair(title, wikitext)
        } catch (e: Exception) {
            Log.e(TAG, "Web clip failed: ${e.message}")
            null
        }
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

            // Handle collisions
            var finalName = safeName
            var counter = 1
            while (attachmentsDir.findFile(finalName) != null && counter < 1000) {
                val baseName = safeName.substringBeforeLast(".")
                val ext = safeName.substringAfterLast(".", "")
                finalName = if (ext.isNotEmpty()) "${baseName}_${counter}.${ext}" else "${baseName}_${counter}"
                counter++
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
        val title = titleEdit?.text?.toString()?.trim() ?: getString(R.string.capture_untitled)
        val tags = tagsEdit?.text?.toString()?.trim() ?: ""
        val now = System.currentTimeMillis()

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

            // Try external attachment: copy file to wiki's attachments folder
            val treeUri = extractTreeUri(wiki.path)
            if (treeUri != null) {
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
                // No folder access, embed as base64
                embedImageAsBase64(captureJson)
            }
        } else if (sharedFileUri != null) {
            // Video/audio file — requires external attachments (too large for base64)
            val mimeType = contentResolver.getType(sharedFileUri!!) ?: intent.type ?: "application/octet-stream"
            val ext = MimeTypeMap.getSingleton().getExtensionFromMimeType(mimeType) ?: "mp4"
            val filename = getDisplayName(sharedFileUri!!) ?: "capture_${now}.${ext}"

            val treeUri = extractTreeUri(wiki.path)
            if (treeUri != null) {
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
        } else {
            // Text capture
            captureJson.put("type", "text/vnd.tiddlywiki")

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
                else -> sharedText ?: ""
            }
            captureJson.put("text", text)

            if (detectedUrl != null) {
                captureJson.put("source_url", detectedUrl)
            }
        }

        // Write capture file
        val capturesDir = File(filesDir, "captures")
        if (!capturesDir.exists()) capturesDir.mkdirs()

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
}
