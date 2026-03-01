package com.burningtreec.tiddlydesktop_rs

import android.content.Context
import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.net.Uri
import android.util.Base64
import android.util.Log
import androidx.documentfile.provider.DocumentFile
import java.io.*
import java.io.File
import java.util.UUID
import java.util.zip.GZIPOutputStream
import java.net.ServerSocket
import java.net.Socket
import java.net.URLDecoder
import java.util.concurrent.ExecutorService
import java.util.concurrent.Executors
import java.util.concurrent.atomic.AtomicBoolean

/**
 * A minimal HTTP server that runs in the WikiActivity's process.
 * Handles serving wiki content and saving changes via SAF.
 *
 * This server is independent of Tauri and runs in the :wiki process,
 * so it continues working even when the landing page is closed.
 *
 * For single-file wikis: Serves the wiki HTML and handles PUT to save.
 * For folder wikis: Serves pre-rendered HTML and handles TiddlyWeb protocol.
 */
class WikiHttpServer(
    private val context: Context,
    private val wikiUri: Uri,
    private val treeUri: Uri?,  // Tree URI for folder wikis or backups
    private val isFolder: Boolean = false,
    private val folderHtmlPath: String? = null,  // Path to pre-rendered HTML for folder wikis
    private val backupsEnabled: Boolean = true,  // Whether to create backups on save
    private val backupCount: Int = 20,  // Max backups to keep (0 = unlimited)
    private val customBackupDirUri: String? = null  // Custom backup directory URI (SAF content:// URI)
) {
    companion object {
        private const val TAG = "WikiHttpServer"
        private var nextPort = 39000
        private val portLock = Any()
        /** Number of extra ports for attachments. With 5 attachment ports + 1 main port,
         *  Chromium gets 36 concurrent connections (6 per host:port). */
        const val ATTACHMENT_PORT_COUNT = 5

        /**
         * Find an available port and bind a ServerSocket to it atomically.
         * Returns the bound ServerSocket (caller takes ownership).
         * The binding happens inside the lock to prevent race conditions
         * when multiple wikis are opened concurrently.
         */
        private fun bindAvailablePort(): ServerSocket {
            synchronized(portLock) {
                for (attempt in 0 until 100) {
                    val port = nextPort++
                    if (nextPort > 39999) nextPort = 39000
                    try {
                        val socket = ServerSocket(port, 50,
                            java.net.InetAddress.getByName("127.0.0.1"))
                        socket.soTimeout = 5000
                        return socket
                    } catch (e: Exception) {
                        // Port in use, try next
                    }
                }
                throw IOException("No available port found")
            }
        }
    }

    private var serverSocket: ServerSocket? = null
    private var attachmentSockets: Array<ServerSocket?> = arrayOfNulls(ATTACHMENT_PORT_COUNT)
    private var executor: ExecutorService? = null
    private val running = AtomicBoolean(false)
    var port: Int = 0
        private set
    var attachmentPorts: IntArray = IntArray(ATTACHMENT_PORT_COUNT)
        private set

    // Per-session authentication token — set as cookie on first wiki load,
    // validated on all subsequent requests to protect against other apps probing localhost
    private val sessionToken: String = UUID.randomUUID().toString()

    // Cached backup directory (lazy-initialized)
    private var backupDirectory: DocumentFile? = null
    private var backupDirectoryChecked = false

    // Background executor for non-critical tasks (backups, cleanup)
    private val backgroundExecutor: ExecutorService = Executors.newSingleThreadExecutor()

    /**
     * Get the wiki filename stem (without .html extension).
     */
    private fun getWikiFilenameStem(): String? {
        return try {
            val wikiFile = DocumentFile.fromSingleUri(context, wikiUri) ?: return null
            val name = wikiFile.name ?: return null
            name.removeSuffix(".html").removeSuffix(".htm")
        } catch (e: Exception) {
            Log.e(TAG, "Failed to get wiki filename: ${e.message}")
            null
        }
    }

    /**
     * Get or create the backup directory for this wiki.
     * Returns null if backups are not available (no tree access).
     */
    private fun getOrCreateBackupDirectory(): DocumentFile? {
        if (backupDirectoryChecked) return backupDirectory

        backupDirectoryChecked = true

        // If custom backup directory is set, use it directly
        if (customBackupDirUri != null) {
            try {
                // The backup dir may be stored as JSON ({"uri":"content://...","documentTopTreeUri":"..."})
                // or as a plain content:// URI string
                val actualUri = if (customBackupDirUri.trimStart().startsWith("{")) {
                    val json = org.json.JSONObject(customBackupDirUri)
                    // Prefer documentTopTreeUri for tree access, fall back to uri
                    val treeUriStr = json.optString("documentTopTreeUri", null)
                    val uriStr = json.optString("uri", null)
                    Uri.parse(if (!treeUriStr.isNullOrEmpty()) treeUriStr else uriStr)
                } else {
                    Uri.parse(customBackupDirUri)
                }
                backupDirectory = DocumentFile.fromTreeUri(context, actualUri)
                if (backupDirectory != null && backupDirectory!!.isDirectory) {
                    Log.d(TAG, "Using custom backup directory: $actualUri")
                    return backupDirectory
                }
                Log.e(TAG, "Custom backup directory not accessible: $actualUri")
                // Fall through to default behavior
            } catch (e: Exception) {
                Log.e(TAG, "Error accessing custom backup directory: ${e.message}")
                // Fall through to default behavior
            }
        }

        // Default: create .backups folder next to wiki file
        val tree = treeUri ?: run {
            Log.d(TAG, "No tree URI - backups disabled")
            return null
        }

        val stem = getWikiFilenameStem() ?: run {
            Log.e(TAG, "Could not determine wiki filename for backups")
            return null
        }

        val backupDirName = "$stem.backups"

        try {
            val parentDir = DocumentFile.fromTreeUri(context, tree) ?: run {
                Log.e(TAG, "Could not access tree URI")
                return null
            }

            // Look for existing backup directory
            backupDirectory = parentDir.findFile(backupDirName)
            if (backupDirectory != null && backupDirectory!!.isDirectory) {
                Log.d(TAG, "Found existing backup directory: $backupDirName")
                return backupDirectory
            }

            // Create backup directory
            backupDirectory = parentDir.createDirectory(backupDirName)
            if (backupDirectory != null) {
                Log.d(TAG, "Created backup directory: $backupDirName")
            } else {
                Log.e(TAG, "Failed to create backup directory: $backupDirName")
            }
            return backupDirectory
        } catch (e: Exception) {
            Log.e(TAG, "Error getting/creating backup directory: ${e.message}")
            return null
        }
    }

    /**
     * Create a backup of the wiki file before saving.
     * Returns true if backup was created successfully (or skipped because disabled).
     */
    private fun createBackup(): Boolean {
        Log.d(TAG, "createBackup() called - backupsEnabled=$backupsEnabled, isFolder=$isFolder, treeUri=$treeUri")
        if (!backupsEnabled) {
            Log.d(TAG, "Backups disabled - skipping")
            return true
        }
        if (isFolder) {
            Log.d(TAG, "Folder wiki - skipping backup")
            return true  // Folder wikis don't need backups (they use git or similar)
        }

        val backupDir = getOrCreateBackupDirectory() ?: run {
            // No backup directory available - continue without backup
            // This happens when user opened wiki without folder access
            Log.w(TAG, "Skipping backup - no backup directory available")
            return true
        }

        val stem = getWikiFilenameStem() ?: return false

        try {
            // Generate timestamped filename
            val timestamp = java.text.SimpleDateFormat("yyyyMMdd-HHmmss", java.util.Locale.US)
                .format(java.util.Date())
            val backupName = "$stem.$timestamp.html"

            // Check if wiki file exists (first save won't have anything to backup)
            val wikiFile = DocumentFile.fromSingleUri(context, wikiUri)
            if (wikiFile == null || !wikiFile.exists()) {
                Log.d(TAG, "Wiki file doesn't exist yet - skipping backup")
                return true
            }

            // Create backup file
            val backupFile = backupDir.createFile("text/html", backupName)
            if (backupFile == null) {
                Log.e(TAG, "Failed to create backup file: $backupName")
                return false
            }

            // Copy content from wiki to backup
            context.contentResolver.openInputStream(wikiUri)?.use { input ->
                context.contentResolver.openOutputStream(backupFile.uri)?.use { output ->
                    input.copyTo(output)
                }
            }

            Log.d(TAG, "Created backup: $backupName")

            // Clean up old backups
            cleanupOldBackups(backupDir, stem)

            return true
        } catch (e: Exception) {
            Log.e(TAG, "Failed to create backup: ${e.message}", e)
            return false
        }
    }

    /**
     * Remove old backups, keeping only the most recent ones.
     */
    private fun cleanupOldBackups(backupDir: DocumentFile, stem: String) {
        if (backupCount == 0) return  // Keep unlimited

        try {
            val prefix = "$stem."
            val backups = backupDir.listFiles()
                .filter { it.isFile && it.name?.startsWith(prefix) == true && it.name?.endsWith(".html") == true }
                .sortedByDescending { it.name }  // Newest first (timestamp sorts correctly)

            // Delete backups beyond the limit
            if (backups.size > backupCount) {
                for (oldBackup in backups.drop(backupCount)) {
                    if (oldBackup.delete()) {
                        Log.d(TAG, "Deleted old backup: ${oldBackup.name}")
                    } else {
                        Log.w(TAG, "Failed to delete old backup: ${oldBackup.name}")
                    }
                }
            }
        } catch (e: Exception) {
            Log.e(TAG, "Error cleaning up old backups: ${e.message}")
        }
    }

    /**
     * Start the HTTP server on an available port.
     * Returns the URL to access the wiki.
     */
    fun start(): String {
        if (running.get()) {
            return "http://127.0.0.1:$port"
        }

        // Bind atomically — port allocation + socket binding in one lock
        serverSocket = bindAvailablePort()
        port = serverSocket!!.localPort

        // Multiple attachment ports (/_file/, /_relative/, /_td/).
        // Chromium limits 6 connections per host:port — with N attachment ports,
        // we get 6*(N+1) total concurrent connections, preventing video/image
        // requests from blocking wiki boot.
        for (i in 0 until ATTACHMENT_PORT_COUNT) {
            attachmentSockets[i] = bindAvailablePort()
            attachmentPorts[i] = attachmentSockets[i]!!.localPort
        }

        executor = Executors.newCachedThreadPool()
        running.set(true)

        Log.d(TAG, "Starting server on port $port (attachments: ${attachmentPorts.joinToString(",")}) for ${wikiUri}")

        // Accept thread for main port (wiki HTML, saves, TiddlyWeb)
        startAcceptThread(serverSocket!!, "WikiHttpServer-$port")

        // Accept threads for attachment ports (media, files, tdlib)
        for (i in 0 until ATTACHMENT_PORT_COUNT) {
            startAcceptThread(attachmentSockets[i]!!, "WikiHttpServer-att-${attachmentPorts[i]}")
        }

        return "http://127.0.0.1:$port"
    }

    /**
     * Start an accept loop on the given ServerSocket, dispatching connections
     * to the shared executor. Used for both main and attachment ports.
     */
    private fun startAcceptThread(sock: ServerSocket, threadName: String) {
        Thread {
            Log.d(TAG, "Accept thread started: $threadName")
            while (running.get()) {
                try {
                    if (sock.isClosed) break
                    val socket = sock.accept()
                    executor?.submit { handleConnection(socket) }
                } catch (e: java.net.SocketTimeoutException) {
                    continue
                } catch (e: java.net.SocketException) {
                    if (running.get() && !sock.isClosed) {
                        Log.e(TAG, "SocketException in $threadName: ${e.message}")
                    }
                    if (sock.isClosed) break
                } catch (e: Exception) {
                    if (running.get()) {
                        Log.e(TAG, "Error in $threadName: ${e.message}", e)
                    }
                }
            }
            Log.d(TAG, "Accept thread exiting: $threadName")
        }.apply {
            name = threadName
            isDaemon = false
        }.start()
    }

    /**
     * Stop the HTTP server (both main and attachment ports).
     */
    fun stop() {
        Log.d(TAG, "Stopping server on port $port (attachments: ${attachmentPorts.joinToString(",")})")
        running.set(false)
        try { serverSocket?.close() } catch (e: Exception) {
            Log.e(TAG, "Error closing server socket: ${e.message}")
        }
        for (i in 0 until ATTACHMENT_PORT_COUNT) {
            try { attachmentSockets[i]?.close() } catch (e: Exception) {
                Log.e(TAG, "Error closing attachment socket $i: ${e.message}")
            }
            attachmentSockets[i] = null
        }
        executor?.shutdownNow()
        backgroundExecutor.shutdown()
        serverSocket = null
        executor = null
    }

    /**
     * Check if the server is running and the socket is valid.
     * The server might think it's running but the socket could be closed
     * (e.g., when the phone goes to sleep and Android kills sockets).
     */
    fun isRunning(): Boolean {
        return running.get() && serverSocket != null && !serverSocket!!.isClosed
    }

    /**
     * Restart the server and return the new URL.
     * Use this when the server socket has died but needs to be revived.
     */
    fun restart(): String {
        Log.d(TAG, "Restarting server...")
        stop()
        return start()
    }

    /**
     * Handle an incoming HTTP connection with keep-alive support.
     * Media routes (/_file/, /_relative/) benefit from connection reuse during
     * video seeking — avoids TCP reconnection overhead on every seek operation.
     */
    private fun handleConnection(socket: Socket) {
        try {
            socket.use { s ->
                // TCP_NODELAY: send headers immediately, don't wait for Nagle's algorithm.
                // Critical for low-latency range responses during video seeking.
                s.tcpNoDelay = true
                s.soTimeout = 30000  // 30 second read timeout
                val input = BufferedInputStream(s.getInputStream(), 8192)
                val output = BufferedOutputStream(s.getOutputStream(), 262144)

                var keepAlive = true
                while (keepAlive) {
                    // Read headers using buffered input (much faster than byte-by-byte)
                    val headerString = readHttpHeaders(input) ?: break
                    val headerLines = headerString.split("\r\n")

                    if (headerLines.isEmpty()) {
                        sendError(output, 400, "Bad Request")
                        break
                    }

                    // Parse request line
                    val requestLine = headerLines[0]
                    val parts = requestLine.split(" ")
                    if (parts.size < 2) {
                        sendError(output, 400, "Bad Request")
                        break
                    }

                    val method = parts[0]
                    val path = parts[1]
                    val httpVersion = if (parts.size >= 3) parts[2] else "HTTP/1.0"

                    // Parse headers
                    val headers = mutableMapOf<String, String>()
                    for (i in 1 until headerLines.size) {
                        val line = headerLines[i]
                        if (line.isEmpty()) continue
                        val colonIndex = line.indexOf(':')
                        if (colonIndex > 0) {
                            val key = line.substring(0, colonIndex).trim().lowercase()
                            val value = line.substring(colonIndex + 1).trim()
                            headers[key] = value
                        }
                    }

                    // Determine keep-alive: HTTP/1.1 defaults to keep-alive
                    val connHeader = headers["connection"]?.lowercase() ?: ""
                    keepAlive = if (httpVersion.contains("1.1")) {
                        !connHeader.contains("close")
                    } else {
                        connHeader.contains("keep-alive")
                    }

                    // Media routes support keep-alive; other routes close after response
                    val isMediaRoute = path.startsWith("/_file/") || path.startsWith("/_relative/") || path.startsWith("/_td/")
                    if (!isMediaRoute) keepAlive = false

                    // Session cookie authentication: protect all routes except
                    // GET / (initial wiki load sets the cookie), HEAD /, and OPTIONS.
                    // Cookie name uses main port (this.port) so it works across
                    // attachment ports too (RFC 6265: cookies are not port-specific).
                    val isPublicRoute = (method == "GET" && path == "/") ||
                        (method == "HEAD" && path == "/") ||
                        method == "OPTIONS"
                    if (!isPublicRoute) {
                        val cookieHeader = headers["cookie"] ?: ""
                        val cookieName = "_td_${port}"
                        val hasValidToken = cookieHeader.split(";").any { cookie ->
                            val trimmed = cookie.trim()
                            trimmed == "$cookieName=$sessionToken"
                        }
                        if (!hasValidToken) {
                            sendError(output, 403, "Forbidden")
                            keepAlive = false
                            continue
                        }
                    }

                    when {
                        method == "GET" && path == "/" -> handleGetWiki(output, headers)
                        method == "HEAD" && path == "/" -> handleHead(output)
                        method == "PUT" && path == "/" -> handlePutWiki(input, output, headers)
                        method == "GET" && path.startsWith("/_file/") -> handleGetFile(output, path, headers, keepAlive)
                        method == "GET" && path.startsWith("/_relative/") -> handleGetRelative(output, path, headers, keepAlive)
                        method == "POST" && path == "/_save-attachment" -> handleSaveAttachment(input, output, headers)
                        method == "OPTIONS" -> handleOptions(output)
                        // TiddlyWeb protocol for folder wikis
                        method == "GET" && path == "/status" -> handleStatus(output)
                        method == "GET" && path == "/recipes/default/tiddlers.json" -> handleGetAllTiddlers(output)
                        method == "GET" && path.startsWith("/recipes/default/tiddlers/") -> handleGetTiddler(output, path)
                        method == "PUT" && path.startsWith("/recipes/default/tiddlers/") -> handlePutTiddler(input, output, headers, path)
                        method == "DELETE" && path.startsWith("/recipes/default/tiddlers/") -> handleDeleteTiddler(output, path)
                        method == "GET" && path.startsWith("/bags/default/tiddlers/") -> handleGetTiddler(output, path.replace("/bags/", "/recipes/"))
                        else -> sendError(output, 404, "Not Found")
                    }

                    // Set idle timeout for next request on keep-alive connections
                    if (keepAlive) {
                        s.soTimeout = 60000  // 60 second idle timeout
                    }
                }
            }
        } catch (e: java.net.SocketTimeoutException) {
            // Keep-alive idle timeout — normal, connection closes silently
        } catch (e: java.net.SocketException) {
            // Connection reset by peer — normal during video seeking
        } catch (e: Exception) {
            Log.e(TAG, "Error handling connection: ${e.message}", e)
        }
    }

    /**
     * Read HTTP headers from a buffered input stream.
     * Returns the header string (up to and including the blank line),
     * or null if the connection was closed.
     * Uses mark/reset to efficiently detect the \r\n\r\n boundary
     * without consuming body data.
     */
    private fun readHttpHeaders(input: BufferedInputStream): String? {
        val buf = ByteArrayOutputStream(2048)
        // State machine to detect \r\n\r\n
        var state = 0  // 0=normal, 1=\r, 2=\r\n, 3=\r\n\r
        while (true) {
            val b = input.read()
            if (b == -1) return null  // Connection closed
            buf.write(b)
            state = when {
                b == '\r'.code && (state == 0 || state == 2) -> state + 1
                b == '\n'.code && state == 1 -> 2
                b == '\n'.code && state == 3 -> break  // Found \r\n\r\n
                else -> 0
            }
        }
        return buf.toString(Charsets.UTF_8.name())
    }

    /**
     * Handle GET / - serve the wiki HTML content.
     * For single-file wikis: reads from wikiUri
     * For folder wikis: reads from folderHtmlPath (pre-rendered by Node.js)
     */
    private fun handleGetWiki(output: OutputStream, reqHeaders: Map<String, String> = emptyMap()) {
        try {
            val t0 = System.currentTimeMillis()
            // Read wiki as raw bytes — avoids String conversion overhead for large wikis.
            // String-based approach allocates ~6x the file size (UTF-16 String + substring
            // concat + toByteArray). Byte-based approach keeps it at ~1x.
            val wikiBytes = if (isFolder && folderHtmlPath != null) {
                val file = File(folderHtmlPath)
                if (file.exists()) file.readBytes()
                else throw IOException("Pre-rendered HTML not found: $folderHtmlPath")
            } else {
                context.contentResolver.openInputStream(wikiUri)?.use {
                    it.readBytes()
                } ?: throw IOException("Failed to read wiki")
            }
            val t1 = System.currentTimeMillis()
            Log.d(TAG, "Wiki read: ${wikiBytes.size} bytes in ${t1 - t0}ms")

            // Build the early URL-transform script that intercepts src attributes on
            // img/video/audio/source BEFORE TiddlyWiki renders them. This ensures attachment
            // requests go to attachment ports from the very start, not the main wiki port.
            val portsJs = attachmentPorts.joinToString(",")
            val earlyTransformScript = """<script>(function(){""" +
                // Attachment port pool
                """var P=[${portsJs}];""" +
                """if(!P.length||!P[0])return;""" +
                // Hash function: same URL → same port (keep-alive friendly)
                """function H(u){var h=0;for(var i=0;i<u.length;i++){h=((h<<5)-h+u.charCodeAt(i))|0;}return h<0?-h:h;}""" +
                // Check if URL should be transformed (relative paths, not http/data/blob)
                """function S(u){if(!u||u.startsWith('data:')||u.startsWith('blob:')||u.startsWith('http://')||u.startsWith('https://'))return false;return true;}""" +
                // Transform: relative path → http://127.0.0.1:{port}/_relative/{encoded}
                """function T(u){if(!S(u))return u;var p=P[H(u)%P.length];return 'http://127.0.0.1:'+p+'/_relative/'+encodeURIComponent(u);}""" +
                // Intercept src property setter on img/video/audio/source
                """['HTMLImageElement','HTMLVideoElement','HTMLAudioElement','HTMLSourceElement'].forEach(function(n){""" +
                """try{var C=window[n];if(!C)return;""" +
                """var d=Object.getOwnPropertyDescriptor(C.prototype,'src');""" +
                """if(!d||!d.set)return;""" +
                """Object.defineProperty(C.prototype,'src',{set:function(v){""" +
                """if(typeof v==='string'&&S(v)){d.set.call(this,T(v));}else{d.set.call(this,v);}""" +
                """},get:d.get,configurable:true,enumerable:true});""" +
                """}catch(e){}});""" +
                // Also intercept setAttribute('src', ...) calls
                """['HTMLImageElement','HTMLVideoElement','HTMLAudioElement','HTMLSourceElement'].forEach(function(n){""" +
                """try{var C=window[n];if(!C)return;""" +
                """var orig=C.prototype.setAttribute;""" +
                """C.prototype.setAttribute=function(a,v){""" +
                """if(a==='src'&&typeof v==='string'&&S(v)){return orig.call(this,a,T(v));}""" +
                """return orig.call(this,a,v);};""" +
                """}catch(e){}});""" +
                // Expose transform for overlay/other scripts
                """window.__tdEarlyTransform=T;window.__tdEarlyTransformCheck=S;""" +
                """})();</script>"""

            // Injection: early URL-transform + iframe referrerpolicy fix + media controls CSS
            val injectionBytes = (
                earlyTransformScript +
                """<script>(function(){function x(u){if(!u||typeof u!=='string')return false;if(u.startsWith('http://127.0.0.1')||u.startsWith('http://localhost'))return false;return u.startsWith('http://')||u.startsWith('https://')||u.startsWith('//');}try{var d=Object.getOwnPropertyDescriptor(HTMLIFrameElement.prototype,'src');if(d&&d.set){Object.defineProperty(HTMLIFrameElement.prototype,'src',{set:function(v){if(x(v))this.referrerPolicy='no-referrer';d.set.call(this,v);},get:d.get,configurable:true,enumerable:true});}}catch(e){}var sa=HTMLIFrameElement.prototype.setAttribute;HTMLIFrameElement.prototype.setAttribute=function(n,v){if(n==='src'&&x(v))this.referrerPolicy='no-referrer';return sa.call(this,n,v);};})();</script>""" +
                """<style id="td-media-controls-css">video{max-width:100%;height:auto;object-fit:contain;border-radius:4px;background:#000;}audio{max-width:100%;width:100%;box-sizing:border-box;}video::-webkit-media-controls-play-button,video::-webkit-media-controls-mute-button,video::-webkit-media-controls-fullscreen-button,video::-webkit-media-controls-overflow-button,video::-webkit-media-controls-timeline,video::-webkit-media-controls-volume-slider,video::-webkit-media-controls-overlay-play-button,audio::-webkit-media-controls-play-button,audio::-webkit-media-controls-mute-button,audio::-webkit-media-controls-timeline,audio::-webkit-media-controls-volume-slider,audio::-webkit-media-controls-overflow-button{cursor:pointer;}video::-webkit-media-controls-overlay-play-button{display:flex;align-items:center;justify-content:center;}</style>"""
            ).toByteArray(Charsets.UTF_8)

            // Find <head> in raw bytes (case-insensitive ASCII scan)
            val headIdx = findTagIgnoreCase(wikiBytes, "<head>")
            val hasInjection = headIdx >= 0

            // Check if client accepts gzip (Android WebView always does)
            val acceptEncoding = reqHeaders["accept-encoding"] ?: ""
            val useGzip = acceptEncoding.contains("gzip")

            if (useGzip) {
                // Gzip: compress into buffer first to get Content-Length.
                // HTML compresses ~80-90%, so a 30MB wiki becomes ~3-5MB.
                val baos = ByteArrayOutputStream(wikiBytes.size / 6)
                GZIPOutputStream(baos).use { gzip ->
                    if (hasInjection) {
                        val insertPos = headIdx + 6 // length of "<head>"
                        gzip.write(wikiBytes, 0, insertPos)
                        gzip.write(injectionBytes)
                        gzip.write(wikiBytes, insertPos, wikiBytes.size - insertPos)
                    } else {
                        gzip.write(wikiBytes)
                    }
                }
                val compressed = baos.toByteArray()
                val t2 = System.currentTimeMillis()
                Log.d(TAG, "Gzip: ${wikiBytes.size} -> ${compressed.size} bytes (${100 - compressed.size * 100 / wikiBytes.size}% saved) in ${t2 - t1}ms")

                val headers = "HTTP/1.1 200 OK\r\n" +
                    "Content-Type: text/html; charset=utf-8\r\n" +
                    "Content-Encoding: gzip\r\n" +
                    "Content-Length: ${compressed.size}\r\n" +
                    "Vary: Accept-Encoding\r\n" +
                    "Set-Cookie: _td_${port}=$sessionToken; Path=/; HttpOnly; SameSite=Strict\r\n" +
                    "Access-Control-Allow-Origin: *\r\n" +
                    "Connection: close\r\n\r\n"
                output.write(headers.toByteArray())
                output.write(compressed)
                val t3 = System.currentTimeMillis()
                Log.d(TAG, "Wiki served: ${t3 - t0}ms total (read=${t1-t0}ms, gzip=${t2-t1}ms, write=${t3-t2}ms)")
            } else {
                // No gzip: stream uncompressed
                val totalLength = wikiBytes.size + if (hasInjection) injectionBytes.size else 0
                val headers = "HTTP/1.1 200 OK\r\n" +
                    "Content-Type: text/html; charset=utf-8\r\n" +
                    "Content-Length: $totalLength\r\n" +
                    "Set-Cookie: _td_${port}=$sessionToken; Path=/; HttpOnly; SameSite=Strict\r\n" +
                    "Access-Control-Allow-Origin: *\r\n" +
                    "Connection: close\r\n\r\n"
                output.write(headers.toByteArray())

                if (hasInjection) {
                    val insertPos = headIdx + 6 // length of "<head>"
                    output.write(wikiBytes, 0, insertPos)
                    output.write(injectionBytes)
                    output.write(wikiBytes, insertPos, wikiBytes.size - insertPos)
                } else {
                    output.write(wikiBytes)
                }
            }
            output.flush()
        } catch (e: Exception) {
            Log.e(TAG, "Error serving wiki: ${e.message}", e)
            sendError(output, 500, "Internal Server Error: ${e.message}")
        }
    }

    /**
     * Find a tag in raw bytes using case-insensitive ASCII comparison.
     * Returns the byte offset of the tag, or -1 if not found.
     */
    private fun findTagIgnoreCase(data: ByteArray, tag: String): Int {
        val tagBytes = tag.lowercase().toByteArray(Charsets.US_ASCII)
        val limit = data.size - tagBytes.size
        var i = 0
        outer@ while (i <= limit) {
            for (j in tagBytes.indices) {
                val b = data[i + j].toInt() and 0xFF
                val lower = if (b in 65..90) (b + 32).toByte() else data[i + j]
                if (lower != tagBytes[j]) {
                    i++
                    continue@outer
                }
            }
            return i
        }
        return -1
    }


    /**
     * Handle PUT / - save the wiki content.
     *
     * Optimized for large wikis:
     * - Streams body directly to SAF output (no full in-memory buffering)
     * - Sends 200 response immediately after writing
     * - Creates backup in background thread after responding
     * - Handles missing Content-Length by reading until EOF
     */
    private fun handlePutWiki(input: InputStream, output: OutputStream, headers: Map<String, String>) {
        try {
            val contentLength = headers["content-length"]?.toLongOrNull() ?: -1L
            val startTime = System.currentTimeMillis()

            Log.d(TAG, "PUT wiki: Content-Length=$contentLength")

            // Stream body directly to SAF file via chunked copy
            var totalWritten = 0L
            context.contentResolver.openOutputStream(wikiUri, "wt")?.use { os ->
                val buf = ByteArray(65536)  // 64KB chunks
                if (contentLength > 0) {
                    // Known length: read exactly contentLength bytes
                    var remaining = contentLength
                    while (remaining > 0) {
                        val toRead = minOf(buf.size.toLong(), remaining).toInt()
                        val read = input.read(buf, 0, toRead)
                        if (read == -1) break
                        os.write(buf, 0, read)
                        totalWritten += read
                        remaining -= read
                    }
                } else {
                    // Unknown length: read until EOF
                    while (true) {
                        val read = input.read(buf)
                        if (read == -1) break
                        os.write(buf, 0, read)
                        totalWritten += read
                    }
                }
            } ?: throw IOException("Failed to open wiki for writing")

            val elapsed = System.currentTimeMillis() - startTime
            Log.d(TAG, "Wiki saved: $totalWritten bytes in ${elapsed}ms")

            // Send response immediately — don't make the client wait for backup
            sendResponse(output, 200, "OK", "text/plain", "Saved".toByteArray())

            // Create backup in background (non-blocking, non-critical)
            backgroundExecutor.submit {
                try {
                    createBackup()
                } catch (e: Exception) {
                    Log.e(TAG, "Background backup failed: ${e.message}", e)
                }
            }
        } catch (e: Exception) {
            Log.e(TAG, "Error saving wiki: ${e.message}", e)
            sendError(output, 500, "Internal Server Error: ${e.message}")
        }
    }

    /**
     * Handle GET /_file/{base64_path} - serve external attachment by absolute path.
     * Supports streaming and HTTP Range requests for large files (videos).
     */
    private fun handleGetFile(output: OutputStream, path: String, headers: Map<String, String> = emptyMap(), keepAlive: Boolean = false) {
        try {
            val encoded = path.removePrefix("/_file/")
            // Decode base64url (- -> +, _ -> /)
            val base64 = encoded.replace('-', '+').replace('_', '/')
            // Add padding if needed
            val padded = when (base64.length % 4) {
                2 -> "$base64=="
                3 -> "$base64="
                else -> base64
            }
            val decodedPath = String(Base64.decode(padded, Base64.DEFAULT))

            Log.d(TAG, "Serving file: $decodedPath")

            val uri = when {
                decodedPath.startsWith("content://") -> Uri.parse(decodedPath)
                decodedPath.startsWith("file://") -> Uri.parse(decodedPath)
                else -> Uri.fromFile(File(decodedPath))
            }

            val contentType = guessMimeType(decodedPath)

            // Convert unsupported image formats to JPEG for WebView display
            if (needsImageConversion(contentType)) {
                streamConvertedImage(output, uri)
                return
            }

            // Get file size
            val fileSize = getFileSize(uri)
            if (fileSize < 0) {
                throw IOException("Cannot determine file size")
            }

            val connValue = if (keepAlive) "keep-alive" else "close"

            // Parse Range header if present
            val rangeHeader = headers["range"]
            if (rangeHeader != null && rangeHeader.startsWith("bytes=")) {
                // Handle range request for video seeking
                handleRangeRequest(output, uri, contentType, fileSize, rangeHeader, connValue)
            } else {
                // Stream the entire file
                streamFile(output, uri, contentType, fileSize, connValue)
            }
        } catch (e: Exception) {
            Log.e(TAG, "Error serving file: ${e.message}", e)
            sendError(output, 404, "Not Found: ${e.message}")
        }
    }

    /**
     * Get the size of a file from its URI.
     */
    private fun getFileSize(uri: Uri): Long {
        return try {
            context.contentResolver.openAssetFileDescriptor(uri, "r")?.use { afd ->
                afd.length
            } ?: -1L
        } catch (e: Exception) {
            Log.e(TAG, "Error getting file size: ${e.message}")
            -1L
        }
    }

    /**
     * Stream a file to the output in chunks.
     */
    private fun streamFile(output: OutputStream, uri: Uri, contentType: String, fileSize: Long, connection: String = "close") {
        val headers = listOf(
            "HTTP/1.1 200 OK",
            "Content-Type: $contentType",
            "Content-Length: $fileSize",
            "Accept-Ranges: bytes",
            "Access-Control-Allow-Origin: *",
            "Connection: $connection",
            "",
            ""
        ).joinToString("\r\n")
        output.write(headers.toByteArray())

        // Stream in 256KB chunks
        val buffer = ByteArray(262144)
        context.contentResolver.openInputStream(uri)?.use { input ->
            var bytesRead: Int
            while (input.read(buffer).also { bytesRead = it } != -1) {
                output.write(buffer, 0, bytesRead)
            }
        }
        output.flush()
    }

    /**
     * Check if an image content type needs conversion for WebView display.
     * WebView supports JPEG, PNG, GIF, WebP, BMP, SVG, ICO natively.
     * Formats like HEIC, HEIF, TIFF, and AVIF can be decoded by Android's
     * BitmapFactory but not rendered by WebView.
     */
    private fun needsImageConversion(contentType: String): Boolean {
        val ct = contentType.lowercase()
        return ct == "image/heic" || ct == "image/heif" ||
               ct == "image/tiff" || ct == "image/avif"
    }

    /**
     * Decode an image from a URI using BitmapFactory and serve it as JPEG.
     * Used for image formats that WebView cannot render natively (HEIC, TIFF, AVIF, etc.).
     */
    private fun streamConvertedImage(output: OutputStream, uri: Uri) {
        val bitmap = context.contentResolver.openInputStream(uri)?.use { input ->
            BitmapFactory.decodeStream(input)
        } ?: throw IOException("Failed to decode image")

        val jpegBytes = ByteArrayOutputStream().use { baos ->
            bitmap.compress(Bitmap.CompressFormat.JPEG, 90, baos)
            bitmap.recycle()
            baos.toByteArray()
        }

        val headers = listOf(
            "HTTP/1.1 200 OK",
            "Content-Type: image/jpeg",
            "Content-Length: ${jpegBytes.size}",
            "Access-Control-Allow-Origin: *",
            "Connection: close",
            "",
            ""
        ).joinToString("\r\n")
        output.write(headers.toByteArray())
        output.write(jpegBytes)
        output.flush()
    }

    /**
     * Handle HTTP Range request for partial content (video seeking).
     */
    private fun handleRangeRequest(output: OutputStream, uri: Uri, contentType: String, fileSize: Long, rangeHeader: String, connection: String = "close") {
        // Parse "bytes=start-end" or "bytes=start-"
        val rangeSpec = rangeHeader.removePrefix("bytes=")
        val rangeParts = rangeSpec.split("-")

        val start = rangeParts[0].toLongOrNull() ?: 0L
        val end = if (rangeParts.size > 1 && rangeParts[1].isNotEmpty()) {
            rangeParts[1].toLongOrNull() ?: (fileSize - 1)
        } else {
            fileSize - 1
        }

        // Validate range
        if (start >= fileSize || start > end) {
            val errorHeaders = listOf(
                "HTTP/1.1 416 Range Not Satisfiable",
                "Content-Range: bytes */$fileSize",
                "Access-Control-Allow-Origin: *",
                "Connection: $connection",
                "",
                ""
            ).joinToString("\r\n")
            output.write(errorHeaders.toByteArray())
            output.flush()
            return
        }

        val contentLength = end - start + 1

        val headers = listOf(
            "HTTP/1.1 206 Partial Content",
            "Content-Type: $contentType",
            "Content-Length: $contentLength",
            "Content-Range: bytes $start-$end/$fileSize",
            "Accept-Ranges: bytes",
            "Access-Control-Allow-Origin: *",
            "Connection: $connection",
            "",
            ""
        ).joinToString("\r\n")
        output.write(headers.toByteArray())

        // Stream the requested range in 256KB chunks using ParcelFileDescriptor for O(1) seeking
        val buffer = ByteArray(262144)
        context.contentResolver.openFileDescriptor(uri, "r")?.use { pfd ->
            val fis = java.io.FileInputStream(pfd.fileDescriptor)
            fis.use { input ->
                // Seek directly to start position (O(1) via file channel)
                input.channel.position(start)

                // Read and send the requested range
                var remaining = contentLength
                while (remaining > 0) {
                    val toRead = minOf(buffer.size.toLong(), remaining).toInt()
                    val bytesRead = input.read(buffer, 0, toRead)
                    if (bytesRead == -1) break
                    output.write(buffer, 0, bytesRead)
                    remaining -= bytesRead
                }
            }
        }
        output.flush()
    }

    /**
     * Handle GET /_relative/{path} - serve file relative to wiki location.
     * Supports streaming and HTTP Range requests for large files (videos).
     */
    private fun handleGetRelative(output: OutputStream, path: String, headers: Map<String, String> = emptyMap(), keepAlive: Boolean = false) {
        try {
            val relativePath = URLDecoder.decode(path.removePrefix("/_relative/"), "UTF-8")
            Log.d(TAG, "Serving relative file: $relativePath")

            // For SAF URIs, we need to use DocumentFile to navigate
            val parentDoc = if (treeUri != null) {
                DocumentFile.fromTreeUri(context, treeUri)
            } else {
                // Try to get parent from single document URI
                DocumentFile.fromSingleUri(context, wikiUri)?.parentFile
            }

            if (parentDoc == null) {
                sendError(output, 404, "Cannot access parent directory")
                return
            }

            // Navigate the path
            val pathParts = relativePath.split("/")
            var currentDoc: DocumentFile? = parentDoc
            for (part in pathParts) {
                if (part.isEmpty() || part == ".") continue
                if (part == "..") {
                    currentDoc = currentDoc?.parentFile
                } else {
                    currentDoc = currentDoc?.findFile(part)
                }
                if (currentDoc == null) break
            }

            if (currentDoc == null || !currentDoc.exists()) {
                sendError(output, 404, "File not found: $relativePath")
                return
            }

            val contentType = currentDoc.type ?: guessMimeType(relativePath)
            val uri = currentDoc.uri

            // Convert unsupported image formats to JPEG for WebView display
            if (needsImageConversion(contentType)) {
                streamConvertedImage(output, uri)
                return
            }

            // Get file size
            val fileSize = getFileSize(uri)
            if (fileSize < 0) {
                throw IOException("Cannot determine file size")
            }

            val connValue = if (keepAlive) "keep-alive" else "close"

            // Parse Range header if present
            val rangeHeader = headers["range"]
            if (rangeHeader != null && rangeHeader.startsWith("bytes=")) {
                // Handle range request for video seeking
                handleRangeRequest(output, uri, contentType, fileSize, rangeHeader, connValue)
            } else {
                // Stream the entire file
                streamFile(output, uri, contentType, fileSize, connValue)
            }
        } catch (e: Exception) {
            Log.e(TAG, "Error serving relative file: ${e.message}", e)
            sendError(output, 404, "Not Found: ${e.message}")
        }
    }

    /**
     * Handle POST /_save-attachment - save an imported file externally.
     * Returns JSON with the path to the saved file.
     */
    private fun handleSaveAttachment(input: InputStream, output: OutputStream, headers: Map<String, String>) {
        try {
            val contentLength = headers["content-length"]?.toIntOrNull() ?: 0
            val filename = headers["x-filename"] ?: "attachment_${System.currentTimeMillis()}"
            val mimeType = headers["content-type"] ?: "application/octet-stream"

            Log.d(TAG, "Saving attachment: $filename ($contentLength bytes, $mimeType)")

            if (contentLength == 0) {
                sendError(output, 400, "Content-Length required")
                return
            }

            // Read the file data
            val buffer = ByteArray(contentLength)
            var totalRead = 0
            while (totalRead < contentLength) {
                val read = input.read(buffer, totalRead, contentLength - totalRead)
                if (read == -1) break
                totalRead += read
            }

            Log.d(TAG, "Read $totalRead bytes for attachment")

            // Find or create the attachments directory
            val attachmentsDir = getOrCreateAttachmentsDir()
            if (attachmentsDir == null) {
                sendError(output, 500, "Cannot create attachments directory. Please grant folder access.")
                return
            }

            // Create a unique filename to avoid collisions
            val safeName = filename.replace("/", "_").replace("\\", "_")
            var targetFile = attachmentsDir.findFile(safeName)
            var finalName = safeName

            if (targetFile != null) {
                // File exists, create unique name
                val baseName = safeName.substringBeforeLast(".")
                val ext = safeName.substringAfterLast(".", "")
                var counter = 1
                do {
                    finalName = if (ext.isNotEmpty()) "${baseName}_$counter.$ext" else "${baseName}_$counter"
                    targetFile = attachmentsDir.findFile(finalName)
                    counter++
                } while (targetFile != null && counter < 1000)
            }

            // Create the file
            targetFile = attachmentsDir.createFile(mimeType, finalName)
            if (targetFile == null) {
                sendError(output, 500, "Failed to create attachment file")
                return
            }

            // Write the data
            context.contentResolver.openOutputStream(targetFile.uri)?.use { os ->
                os.write(buffer, 0, totalRead)
            } ?: throw IOException("Failed to write attachment")

            Log.d(TAG, "Attachment saved: ${targetFile.uri}")

            // Return the relative path for _canonical_uri
            val relativePath = "attachments/$finalName"
            val response = """{"success":true,"path":"$relativePath","uri":"${escapeJson(targetFile.uri.toString())}"}"""
            sendResponse(output, 200, "OK", "application/json", response.toByteArray())

        } catch (e: Exception) {
            Log.e(TAG, "Error saving attachment: ${e.message}", e)
            sendError(output, 500, "Failed to save attachment: ${e.message}")
        }
    }

    /**
     * Get or create the attachments directory next to the wiki.
     */
    private fun getOrCreateAttachmentsDir(): DocumentFile? {
        // Need tree access to create directories
        if (treeUri == null) {
            Log.w(TAG, "No tree URI available - cannot create attachments directory")
            return null
        }

        val parentDoc = DocumentFile.fromTreeUri(context, treeUri)
        if (parentDoc == null) {
            Log.e(TAG, "Cannot access tree URI")
            return null
        }

        // Look for existing attachments directory
        var attachmentsDir = parentDoc.findFile("attachments")
        if (attachmentsDir != null && attachmentsDir.isDirectory) {
            return attachmentsDir
        }

        // Create the directory
        attachmentsDir = parentDoc.createDirectory("attachments")
        if (attachmentsDir == null) {
            Log.e(TAG, "Failed to create attachments directory")
            return null
        }

        Log.d(TAG, "Created attachments directory: ${attachmentsDir.uri}")
        return attachmentsDir
    }

    /**
     * Handle HEAD / - simple health check response.
     */
    private fun handleHead(output: OutputStream) {
        val headers = listOf(
            "HTTP/1.1 200 OK",
            "Content-Type: text/html; charset=utf-8",
            "Content-Length: 0",
            "Access-Control-Allow-Origin: *",
            "Connection: close",
            "",
            ""
        ).joinToString("\r\n")
        output.write(headers.toByteArray())
        output.flush()
    }

    /**
     * Handle OPTIONS requests for CORS.
     */
    private fun handleOptions(output: OutputStream) {
        val headers = listOf(
            "HTTP/1.1 204 No Content",
            "Access-Control-Allow-Origin: *",
            "Access-Control-Allow-Methods: GET, PUT, POST, DELETE, OPTIONS",
            "Access-Control-Allow-Headers: Content-Type, X-Filename",
            "Content-Length: 0",
            "",
            ""
        ).joinToString("\r\n")
        output.write(headers.toByteArray())
        output.flush()
    }

    /**
     * Send an HTTP response.
     */
    private fun sendResponse(output: OutputStream, code: Int, status: String, contentType: String, body: ByteArray) {
        val headers = listOf(
            "HTTP/1.1 $code $status",
            "Content-Type: $contentType",
            "Content-Length: ${body.size}",
            "Access-Control-Allow-Origin: *",
            "Connection: close",
            "",
            ""
        ).joinToString("\r\n")
        output.write(headers.toByteArray())
        output.write(body)
        output.flush()
    }

    /**
     * Send an HTTP error response.
     */
    private fun sendError(output: OutputStream, code: Int, message: String) {
        val body = message.toByteArray()
        val headers = listOf(
            "HTTP/1.1 $code $message",
            "Content-Type: text/plain",
            "Content-Length: ${body.size}",
            "Access-Control-Allow-Origin: *",
            "Connection: close",
            "",
            ""
        ).joinToString("\r\n")
        output.write(headers.toByteArray())
        output.write(body)
        output.flush()
    }

    // ============ TiddlyWeb Protocol Handlers (for folder wikis) ============

    /**
     * Handle GET /status - TiddlyWeb status endpoint.
     */
    private fun handleStatus(output: OutputStream) {
        val status = """{"username":"TiddlyDesktop","space":{"recipe":"default"},"tiddlywiki_version":"5.3.0"}"""
        sendResponse(output, 200, "OK", "application/json", status.toByteArray())
    }

    /**
     * Handle GET /recipes/default/tiddlers.json - list all tiddlers (skinny).
     */
    private fun handleGetAllTiddlers(output: OutputStream) {
        if (!isFolder) {
            sendError(output, 404, "Not a folder wiki")
            return
        }

        try {
            val tiddlers = mutableListOf<Map<String, Any?>>()
            val wikiDoc = DocumentFile.fromTreeUri(context, wikiUri) ?: DocumentFile.fromSingleUri(context, wikiUri)

            if (wikiDoc == null) {
                sendError(output, 500, "Cannot access wiki folder")
                return
            }

            // Find the tiddlers directory
            val tiddlersDir = wikiDoc.findFile("tiddlers")
            if (tiddlersDir != null && tiddlersDir.isDirectory) {
                for (file in tiddlersDir.listFiles()) {
                    if (file.isFile) {
                        val name = file.name ?: continue
                        if (name.endsWith(".tid") || name.endsWith(".meta")) {
                            val tiddler = parseTidFile(file)
                            if (tiddler != null) {
                                // Return skinny tiddler (no text)
                                val skinny = tiddler.toMutableMap()
                                skinny.remove("text")
                                tiddlers.add(skinny)
                            }
                        }
                    }
                }
            }

            val json = tiddlersToJson(tiddlers)
            sendResponse(output, 200, "OK", "application/json", json.toByteArray())
        } catch (e: Exception) {
            Log.e(TAG, "Error listing tiddlers: ${e.message}", e)
            sendError(output, 500, "Internal Server Error: ${e.message}")
        }
    }

    /**
     * Handle GET /recipes/default/tiddlers/{title} - get a single tiddler.
     */
    private fun handleGetTiddler(output: OutputStream, path: String) {
        if (!isFolder) {
            sendError(output, 404, "Not a folder wiki")
            return
        }

        try {
            val title = URLDecoder.decode(path.substringAfter("/recipes/default/tiddlers/"), "UTF-8")
            Log.d(TAG, "Getting tiddler: $title")

            val tidFile = findTiddlerFile(title)
            if (tidFile == null) {
                sendError(output, 404, "Tiddler not found: $title")
                return
            }

            val tiddler = parseTidFile(tidFile)
            if (tiddler == null) {
                sendError(output, 500, "Failed to parse tiddler")
                return
            }

            val json = tiddlerToJson(tiddler)
            sendResponse(output, 200, "OK", "application/json", json.toByteArray())
        } catch (e: Exception) {
            Log.e(TAG, "Error getting tiddler: ${e.message}", e)
            sendError(output, 500, "Internal Server Error: ${e.message}")
        }
    }

    /**
     * Handle PUT /recipes/default/tiddlers/{title} - create or update a tiddler.
     */
    private fun handlePutTiddler(input: InputStream, output: OutputStream, headers: Map<String, String>, path: String) {
        if (!isFolder) {
            sendError(output, 404, "Not a folder wiki")
            return
        }

        try {
            val title = URLDecoder.decode(path.substringAfter("/recipes/default/tiddlers/"), "UTF-8")
            Log.d(TAG, "Putting tiddler: $title")

            val contentLength = headers["content-length"]?.toIntOrNull() ?: 0
            val buffer = ByteArray(contentLength)
            var totalRead = 0
            while (totalRead < contentLength) {
                val read = input.read(buffer, totalRead, contentLength - totalRead)
                if (read == -1) break
                totalRead += read
            }
            val jsonStr = String(buffer, 0, totalRead, Charsets.UTF_8)

            // Parse JSON to get tiddler fields
            val tiddler = jsonToTiddler(jsonStr)
            if (tiddler == null) {
                sendError(output, 400, "Invalid tiddler JSON")
                return
            }

            // Write to .tid file
            saveTiddler(title, tiddler)

            sendResponse(output, 204, "No Content", "application/json", ByteArray(0))
        } catch (e: Exception) {
            Log.e(TAG, "Error putting tiddler: ${e.message}", e)
            sendError(output, 500, "Internal Server Error: ${e.message}")
        }
    }

    /**
     * Handle DELETE /recipes/default/tiddlers/{title} - delete a tiddler.
     */
    private fun handleDeleteTiddler(output: OutputStream, path: String) {
        if (!isFolder) {
            sendError(output, 404, "Not a folder wiki")
            return
        }

        try {
            val title = URLDecoder.decode(path.substringAfter("/recipes/default/tiddlers/"), "UTF-8")
            Log.d(TAG, "Deleting tiddler: $title")

            val tidFile = findTiddlerFile(title)
            if (tidFile == null) {
                sendError(output, 404, "Tiddler not found: $title")
                return
            }

            if (!tidFile.delete()) {
                sendError(output, 500, "Failed to delete tiddler file")
                return
            }

            sendResponse(output, 204, "No Content", "text/plain", ByteArray(0))
        } catch (e: Exception) {
            Log.e(TAG, "Error deleting tiddler: ${e.message}", e)
            sendError(output, 500, "Internal Server Error: ${e.message}")
        }
    }

    // ============ Tiddler File Handling ============

    /**
     * Find a tiddler file by title.
     */
    private fun findTiddlerFile(title: String): DocumentFile? {
        val wikiDoc = DocumentFile.fromTreeUri(context, wikiUri) ?: return null
        val tiddlersDir = wikiDoc.findFile("tiddlers") ?: return null

        // Sanitize title for filename
        val safeTitle = title.replace("/", "_").replace("\\", "_").replace(":", "_")

        // Try .tid extension first
        var file = tiddlersDir.findFile("$safeTitle.tid")
        if (file != null) return file

        // Try with $ encoded as _
        val encodedTitle = safeTitle.replace("$", "_")
        file = tiddlersDir.findFile("$encodedTitle.tid")
        if (file != null) return file

        // Search all files for matching title
        for (f in tiddlersDir.listFiles()) {
            if (f.isFile && f.name?.endsWith(".tid") == true) {
                val parsed = parseTidFile(f)
                if (parsed?.get("title") == title) {
                    return f
                }
            }
        }

        return null
    }

    /**
     * Parse a .tid file into a map of fields.
     */
    private fun parseTidFile(file: DocumentFile): Map<String, Any?>? {
        return try {
            val content = context.contentResolver.openInputStream(file.uri)?.use {
                it.bufferedReader().readText()
            } ?: return null

            val fields = mutableMapOf<String, Any?>()
            val lines = content.lines()
            var inBody = false
            val bodyLines = mutableListOf<String>()

            for (line in lines) {
                if (inBody) {
                    bodyLines.add(line)
                } else if (line.isEmpty()) {
                    inBody = true
                } else {
                    val colonIndex = line.indexOf(':')
                    if (colonIndex > 0) {
                        val key = line.substring(0, colonIndex).trim()
                        val value = line.substring(colonIndex + 1).trim()
                        fields[key] = value
                    }
                }
            }

            fields["text"] = bodyLines.joinToString("\n")

            // Add revision based on file modification time
            fields["revision"] = file.lastModified().toString()

            fields
        } catch (e: Exception) {
            Log.e(TAG, "Error parsing .tid file: ${e.message}")
            null
        }
    }

    /**
     * Save a tiddler to a .tid file.
     */
    private fun saveTiddler(title: String, fields: Map<String, Any?>) {
        val wikiDoc = DocumentFile.fromTreeUri(context, wikiUri) ?: throw IOException("Cannot access wiki folder")
        var tiddlersDir = wikiDoc.findFile("tiddlers")
        if (tiddlersDir == null) {
            tiddlersDir = wikiDoc.createDirectory("tiddlers") ?: throw IOException("Cannot create tiddlers directory")
        }

        // Sanitize title for filename
        val safeTitle = title.replace("/", "_").replace("\\", "_").replace(":", "_").replace("$", "_")
        val filename = "$safeTitle.tid"

        // Check if file exists
        var file = tiddlersDir.findFile(filename)
        if (file == null) {
            file = tiddlersDir.createFile("text/plain", filename) ?: throw IOException("Cannot create tiddler file")
        }

        // Build .tid content
        val sb = StringBuilder()

        // Write fields (except text)
        for ((key, value) in fields) {
            if (key != "text" && key != "revision" && value != null) {
                sb.append("$key: $value\n")
            }
        }

        // Empty line separates fields from body
        sb.append("\n")

        // Write text body
        val text = fields["text"]?.toString() ?: ""
        sb.append(text)

        // Write to file
        context.contentResolver.openOutputStream(file.uri, "wt")?.use { os ->
            os.write(sb.toString().toByteArray(Charsets.UTF_8))
        } ?: throw IOException("Cannot write to tiddler file")
    }

    // ============ JSON Helpers ============

    /**
     * Convert a list of tiddlers to JSON array.
     */
    private fun tiddlersToJson(tiddlers: List<Map<String, Any?>>): String {
        val sb = StringBuilder("[")
        tiddlers.forEachIndexed { index, tiddler ->
            if (index > 0) sb.append(",")
            sb.append(tiddlerToJson(tiddler))
        }
        sb.append("]")
        return sb.toString()
    }

    /**
     * Convert a single tiddler to JSON.
     */
    private fun tiddlerToJson(tiddler: Map<String, Any?>): String {
        val sb = StringBuilder("{")
        var first = true
        for ((key, value) in tiddler) {
            if (value != null) {
                if (!first) sb.append(",")
                first = false
                sb.append("\"${escapeJson(key)}\":\"${escapeJson(value.toString())}\"")
            }
        }
        sb.append("}")
        return sb.toString()
    }

    /**
     * Parse JSON to a tiddler map.
     */
    private fun jsonToTiddler(json: String): Map<String, Any?>? {
        return try {
            val result = mutableMapOf<String, Any?>()
            // Simple JSON parsing (for basic tiddler objects)
            val trimmed = json.trim()
            if (!trimmed.startsWith("{") || !trimmed.endsWith("}")) return null

            val content = trimmed.substring(1, trimmed.length - 1)

            // Split by commas, but not inside quotes
            var inQuote = false
            var escaped = false
            var current = StringBuilder()
            val pairs = mutableListOf<String>()

            for (c in content) {
                when {
                    escaped -> {
                        current.append(c)
                        escaped = false
                    }
                    c == '\\' -> {
                        current.append(c)
                        escaped = true
                    }
                    c == '"' -> {
                        current.append(c)
                        inQuote = !inQuote
                    }
                    c == ',' && !inQuote -> {
                        pairs.add(current.toString())
                        current = StringBuilder()
                    }
                    else -> current.append(c)
                }
            }
            if (current.isNotEmpty()) pairs.add(current.toString())

            for (pair in pairs) {
                val colonIndex = pair.indexOf(':')
                if (colonIndex > 0) {
                    val key = pair.substring(0, colonIndex).trim().removeSurrounding("\"")
                    val value = pair.substring(colonIndex + 1).trim().removeSurrounding("\"")
                    result[key] = unescapeJson(value)
                }
            }

            result
        } catch (e: Exception) {
            Log.e(TAG, "Error parsing JSON: ${e.message}")
            null
        }
    }

    /**
     * Escape a string for JSON.
     */
    private fun escapeJson(s: String): String {
        return s.replace("\\", "\\\\")
            .replace("\"", "\\\"")
            .replace("\n", "\\n")
            .replace("\r", "\\r")
            .replace("\t", "\\t")
    }

    /**
     * Unescape a JSON string.
     */
    private fun unescapeJson(s: String): String {
        return s.replace("\\n", "\n")
            .replace("\\r", "\r")
            .replace("\\t", "\t")
            .replace("\\\"", "\"")
            .replace("\\\\", "\\")
    }

    /**
     * Guess MIME type from file extension.
     */
    private fun guessMimeType(path: String): String {
        val ext = path.substringAfterLast('.', "").lowercase()
        return when (ext) {
            // Text/markup
            "html", "htm" -> "text/html"
            "css" -> "text/css"
            "js" -> "application/javascript"
            "json" -> "application/json"
            "txt" -> "text/plain"
            "xml" -> "application/xml"
            "md" -> "text/markdown"
            "tid" -> "text/vnd.tiddlywiki"
            // Images
            "png" -> "image/png"
            "jpg", "jpeg" -> "image/jpeg"
            "gif" -> "image/gif"
            "svg" -> "image/svg+xml"
            "webp" -> "image/webp"
            "ico" -> "image/x-icon"
            "bmp" -> "image/bmp"
            "tiff", "tif" -> "image/tiff"
            "heic", "heif" -> "image/heic"
            // Audio
            "mp3" -> "audio/mpeg"
            "m4a" -> "audio/mp4"
            "aac" -> "audio/aac"
            "ogg", "oga" -> "audio/ogg"
            "opus" -> "audio/opus"
            "wav" -> "audio/wav"
            "flac" -> "audio/flac"
            "aiff", "aif" -> "audio/aiff"
            "wma" -> "audio/x-ms-wma"
            "mid", "midi" -> "audio/midi"
            // Video
            "mp4", "m4v" -> "video/mp4"
            "webm" -> "video/webm"
            "ogv" -> "video/ogg"
            "avi" -> "video/x-msvideo"
            "mov" -> "video/quicktime"
            "wmv" -> "video/x-ms-wmv"
            "mkv" -> "video/x-matroska"
            "3gp" -> "video/3gpp"
            // Fonts
            "woff" -> "font/woff"
            "woff2" -> "font/woff2"
            "ttf" -> "font/ttf"
            "otf" -> "font/otf"
            "eot" -> "application/vnd.ms-fontobject"
            // Documents
            "pdf" -> "application/pdf"
            "doc" -> "application/msword"
            "docx" -> "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
            "xls" -> "application/vnd.ms-excel"
            "xlsx" -> "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
            "ppt" -> "application/vnd.ms-powerpoint"
            "pptx" -> "application/vnd.openxmlformats-officedocument.presentationml.presentation"
            // Archives
            "zip" -> "application/zip"
            "tar" -> "application/x-tar"
            "gz", "gzip" -> "application/gzip"
            "rar" -> "application/vnd.rar"
            "7z" -> "application/x-7z-compressed"
            else -> "application/octet-stream"
        }
    }
}
