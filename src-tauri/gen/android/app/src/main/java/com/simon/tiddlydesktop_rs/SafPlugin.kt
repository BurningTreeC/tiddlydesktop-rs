package com.simon.tiddlydesktop_rs

import android.app.Activity
import android.database.Cursor
import android.net.Uri
import android.provider.DocumentsContract
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSArray
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin

@InvokeArg
class ListDirectoryArgs {
    lateinit var uri: String
}

@InvokeArg
class FileExistsArgs {
    lateinit var parentUri: String
    lateinit var fileName: String
}

@TauriPlugin
class SafPlugin(private val activity: Activity): Plugin(activity) {

    @Command
    fun listDirectory(invoke: Invoke) {
        val args = invoke.parseArgs(ListDirectoryArgs::class.java)
        val res = JSObject()
        val entries = JSArray()

        try {
            val treeUri = Uri.parse(args.uri)
            val documentId = DocumentsContract.getTreeDocumentId(treeUri)
            val childrenUri = DocumentsContract.buildChildDocumentsUriUsingTree(treeUri, documentId)

            val projection = arrayOf(
                DocumentsContract.Document.COLUMN_DISPLAY_NAME,
                DocumentsContract.Document.COLUMN_DOCUMENT_ID,
                DocumentsContract.Document.COLUMN_MIME_TYPE
            )

            val cursor: Cursor? = activity.contentResolver.query(
                childrenUri,
                projection,
                null,
                null,
                null
            )

            cursor?.use {
                val nameIndex = it.getColumnIndex(DocumentsContract.Document.COLUMN_DISPLAY_NAME)
                val idIndex = it.getColumnIndex(DocumentsContract.Document.COLUMN_DOCUMENT_ID)
                val mimeIndex = it.getColumnIndex(DocumentsContract.Document.COLUMN_MIME_TYPE)

                while (it.moveToNext()) {
                    val name = it.getString(nameIndex)
                    val docId = it.getString(idIndex)
                    val mimeType = it.getString(mimeIndex)
                    val isDirectory = mimeType == DocumentsContract.Document.MIME_TYPE_DIR

                    val entry = JSObject()
                    entry.put("name", name)
                    entry.put("documentId", docId)
                    entry.put("isFile", !isDirectory)
                    entry.put("mimeType", mimeType)

                    // Build the URI for this child document
                    val childUri = DocumentsContract.buildDocumentUriUsingTree(treeUri, docId)
                    entry.put("uri", childUri.toString())

                    entries.put(entry)
                }
            }

            res.put("entries", entries)
            res.put("success", true)
        } catch (e: Exception) {
            res.put("success", false)
            res.put("error", e.message ?: "Unknown error")
            res.put("entries", entries)
        }

        invoke.resolve(res)
    }

    @Command
    fun listSubdirectory(invoke: Invoke) {
        // For listing a subdirectory when we have the parent tree URI and child document ID
        val args = invoke.parseArgs(ListDirectoryArgs::class.java)
        val res = JSObject()
        val entries = JSArray()

        try {
            // Parse as document URI (not tree URI)
            val docUri = Uri.parse(args.uri)

            // Extract tree URI and document ID from the path
            // content://com.android.externalstorage.documents/tree/primary%3ADocuments/document/primary%3ADocuments%2Fsubfolder
            val treeDocId = DocumentsContract.getTreeDocumentId(docUri)
            val docId = DocumentsContract.getDocumentId(docUri)

            // Rebuild as tree URI for listing children
            val authority = docUri.authority
            val treeUri = DocumentsContract.buildTreeDocumentUri(authority, treeDocId)
            val childrenUri = DocumentsContract.buildChildDocumentsUriUsingTree(treeUri, docId)

            val projection = arrayOf(
                DocumentsContract.Document.COLUMN_DISPLAY_NAME,
                DocumentsContract.Document.COLUMN_DOCUMENT_ID,
                DocumentsContract.Document.COLUMN_MIME_TYPE
            )

            val cursor: Cursor? = activity.contentResolver.query(
                childrenUri,
                projection,
                null,
                null,
                null
            )

            cursor?.use {
                val nameIndex = it.getColumnIndex(DocumentsContract.Document.COLUMN_DISPLAY_NAME)
                val idIndex = it.getColumnIndex(DocumentsContract.Document.COLUMN_DOCUMENT_ID)
                val mimeIndex = it.getColumnIndex(DocumentsContract.Document.COLUMN_MIME_TYPE)

                while (it.moveToNext()) {
                    val name = it.getString(nameIndex)
                    val childDocId = it.getString(idIndex)
                    val mimeType = it.getString(mimeIndex)
                    val isDirectory = mimeType == DocumentsContract.Document.MIME_TYPE_DIR

                    val entry = JSObject()
                    entry.put("name", name)
                    entry.put("documentId", childDocId)
                    entry.put("isFile", !isDirectory)
                    entry.put("mimeType", mimeType)

                    // Build the URI for this child document
                    val childUri = DocumentsContract.buildDocumentUriUsingTree(treeUri, childDocId)
                    entry.put("uri", childUri.toString())

                    entries.put(entry)
                }
            }

            res.put("entries", entries)
            res.put("success", true)
        } catch (e: Exception) {
            res.put("success", false)
            res.put("error", e.message ?: "Unknown error")
            res.put("entries", entries)
        }

        invoke.resolve(res)
    }

    @Command
    fun fileExistsInDirectory(invoke: Invoke) {
        val args = invoke.parseArgs(FileExistsArgs::class.java)
        val res = JSObject()

        try {
            val treeUri = Uri.parse(args.parentUri)
            val documentId = DocumentsContract.getTreeDocumentId(treeUri)
            val childrenUri = DocumentsContract.buildChildDocumentsUriUsingTree(treeUri, documentId)

            val projection = arrayOf(
                DocumentsContract.Document.COLUMN_DISPLAY_NAME
            )

            val cursor: Cursor? = activity.contentResolver.query(
                childrenUri,
                projection,
                null,
                null,
                null
            )

            var found = false
            cursor?.use {
                val nameIndex = it.getColumnIndex(DocumentsContract.Document.COLUMN_DISPLAY_NAME)
                while (it.moveToNext()) {
                    val name = it.getString(nameIndex)
                    if (name == args.fileName) {
                        found = true
                        break
                    }
                }
            }

            res.put("exists", found)
            res.put("success", true)
        } catch (e: Exception) {
            res.put("exists", false)
            res.put("success", false)
            res.put("error", e.message ?: "Unknown error")
        }

        invoke.resolve(res)
    }
}
