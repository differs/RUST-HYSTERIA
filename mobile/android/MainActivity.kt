package dev.dioxus.main

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.content.Context
import android.content.Intent
import android.content.SharedPreferences
import android.net.Uri
import android.net.VpnService
import android.os.Build
import android.os.Bundle
import android.os.ParcelFileDescriptor
import android.provider.OpenableColumns
import android.util.Log
import androidx.core.app.NotificationCompat
import java.io.File
import java.io.IOException
import java.util.concurrent.atomic.AtomicBoolean

private const val HY_LOG_TAG = "HysteriaMobile"

data class ManagedRuntimeProfile(
    val server: String,
    val auth: String,
    val obfsPassword: String,
    val sni: String,
    val caPath: String,
    val pinSha256: String,
    val bandwidthUp: String,
    val bandwidthDown: String,
    val quicInitStreamReceiveWindow: String,
    val quicMaxStreamReceiveWindow: String,
    val quicInitConnectionReceiveWindow: String,
    val quicMaxConnectionReceiveWindow: String,
    val quicMaxIdleTimeout: String,
    val quicKeepAlivePeriod: String,
    val quicDisablePathMtuDiscovery: Boolean,
    val insecureTls: Boolean,
)

data class ManagedRuntimeRecord(
    val profile: ManagedRuntimeProfile,
    val socksHost: String,
    val socksPort: Int,
)

class MainActivity : WryActivity() {
    private fun log(message: String) {
        Log.i(HY_LOG_TAG, message)
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        appContext = applicationContext
        HysteriaVpnService.registerAppContext(applicationContext)
        instance = this
        log("onCreate intent=$intent extras=${intent?.extras}")
        cacheLaunchExtras(intent)
        primeAndroidActivityBridge()
    }

    override fun onNewIntent(intent: Intent?) {
        super.onNewIntent(intent)
        setIntent(intent)
        HysteriaVpnService.registerAppContext(applicationContext)
        log("onNewIntent intent=$intent extras=${intent?.extras}")
        cacheLaunchExtras(intent)
        primeAndroidActivityBridge()
    }

    override fun onDestroy() {
        if (instance === this) {
            instance = null
        }
        super.onDestroy()
    }

    @Deprecated("Deprecated in Java")
    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        super.onActivityResult(requestCode, resultCode, data)
        when (requestCode) {
            VPN_REQUEST_CODE -> {
                permissionGranted.set(resultCode == RESULT_OK || VpnService.prepare(this) == null)
            }
            CONFIG_IMPORT_REQUEST_CODE -> {
                if (resultCode != RESULT_OK || data?.data == null) {
                    return
                }
                runCatching { handleConfigImportResult(data.data!!) }
                    .onFailure {
                        Log.e(HY_LOG_TAG, "handleConfigImportResult failed", it)
                        setPendingConfigImportResult(
                            "err${IMPORT_RESULT_DELIMITER}${it.message ?: "failed to import config"}"
                        )
                    }
            }
            CA_IMPORT_REQUEST_CODE -> {
                if (resultCode != RESULT_OK || data?.data == null) {
                    return
                }
                runCatching { handleCaImportResult(data.data!!) }
                    .onFailure {
                        Log.e(HY_LOG_TAG, "handleCaImportResult failed", it)
                        setPendingCaImportResult(
                            "err${IMPORT_RESULT_DELIMITER}${it.message ?: "failed to import certificate"}"
                        )
                    }
            }
        }
    }

    fun requestVpnPermissionFromRust() {
        runOnUiThread {
            log("requestVpnPermissionFromRust on UI thread")
            val intent = VpnService.prepare(this)
            if (intent == null) {
                permissionGranted.set(true)
                return@runOnUiThread
            }
            startActivityForResult(intent, VPN_REQUEST_CODE)
        }
    }

    fun startVpnServiceFromRust(socksHost: String, socksPort: Int) {
        runOnUiThread {
            log("startVpnServiceFromRust socks=$socksHost:$socksPort on UI thread")
            if (VpnService.prepare(this) != null) {
                requestVpnPermissionFromRust()
                return@runOnUiThread
            }
            permissionGranted.set(true)
            HysteriaVpnService.startShellOnly(this, socksHost, socksPort)
        }
    }

    fun startManagedVpnFromRust(
        profile: ManagedRuntimeProfile,
        socksHost: String,
        socksPort: Int,
    ) {
        runOnUiThread {
            log("startManagedVpnFromRust server=${profile.server} socks=$socksHost:$socksPort on UI thread")
            if (VpnService.prepare(this) != null) {
                requestVpnPermissionFromRust()
                return@runOnUiThread
            }
            permissionGranted.set(true)
            HysteriaVpnService.startManaged(this, profile, socksHost, socksPort)
        }
    }

    fun stopVpnServiceFromRust() {
        runOnUiThread {
            log("stopVpnServiceFromRust on UI thread")
            HysteriaVpnService.stopManaged(applicationContext)
        }
    }

    fun isVpnPermissionGrantedFromRust(): Boolean {
        return permissionGranted.get() || VpnService.prepare(this) == null
    }

    fun isVpnActiveFromRust(): Boolean {
        return HysteriaVpnService.isActive()
    }

    fun protectVpnFdFromRust(fd: Int): Boolean {
        return HysteriaVpnService.protectFdFromRust(fd)
    }

    fun takeVpnTunFdFromRust(): Int {
        return HysteriaVpnService.dupTunFdForRust()
    }

    fun launchStringExtraFromRust(key: String): String? {
        val value = currentIntentStringExtra(key)
        log("launchStringExtraFromRust key=$key value=${if (value.isNullOrEmpty()) "<empty>" else value}")
        return value
    }

    fun launchBooleanExtraFromRust(key: String): Boolean {
        val value = currentIntentBooleanExtra(key)
        log("launchBooleanExtraFromRust key=$key value=$value")
        return value
    }

    fun startManagedVpnFieldsFromRust(
        server: String,
        auth: String,
        obfsPassword: String,
        sni: String,
        caPath: String,
        pinSha256: String,
        bandwidthUp: String,
        bandwidthDown: String,
        quicInitStreamReceiveWindow: String,
        quicMaxStreamReceiveWindow: String,
        quicInitConnectionReceiveWindow: String,
        quicMaxConnectionReceiveWindow: String,
        quicMaxIdleTimeout: String,
        quicKeepAlivePeriod: String,
        quicDisablePathMtuDiscovery: Boolean,
        insecureTls: Boolean,
        socksHost: String,
        socksPort: Int,
    ) {
        startManagedVpnFromRust(
            ManagedRuntimeProfile(
                server = server,
                auth = auth,
                obfsPassword = obfsPassword,
                sni = sni,
                caPath = caPath,
                pinSha256 = pinSha256,
                bandwidthUp = bandwidthUp,
                bandwidthDown = bandwidthDown,
                quicInitStreamReceiveWindow = quicInitStreamReceiveWindow,
                quicMaxStreamReceiveWindow = quicMaxStreamReceiveWindow,
                quicInitConnectionReceiveWindow = quicInitConnectionReceiveWindow,
                quicMaxConnectionReceiveWindow = quicMaxConnectionReceiveWindow,
                quicMaxIdleTimeout = quicMaxIdleTimeout,
                quicKeepAlivePeriod = quicKeepAlivePeriod,
                quicDisablePathMtuDiscovery = quicDisablePathMtuDiscovery,
                insecureTls = insecureTls,
            ),
            socksHost,
            socksPort,
        )
    }

    fun saveProfileFromRust(
        server: String,
        auth: String,
        obfsPassword: String,
        sni: String,
        caPath: String,
        pinSha256: String,
        bandwidthUp: String,
        bandwidthDown: String,
        quicInitStreamReceiveWindow: String,
        quicMaxStreamReceiveWindow: String,
        quicInitConnectionReceiveWindow: String,
        quicMaxConnectionReceiveWindow: String,
        quicMaxIdleTimeout: String,
        quicKeepAlivePeriod: String,
        quicDisablePathMtuDiscovery: Boolean,
        insecureTls: Boolean,
    ) {
        HysteriaVpnService.saveProfile(
            applicationContext,
            ManagedRuntimeProfile(
                server = server,
                auth = auth,
                obfsPassword = obfsPassword,
                sni = sni,
                caPath = caPath,
                pinSha256 = pinSha256,
                bandwidthUp = bandwidthUp,
                bandwidthDown = bandwidthDown,
                quicInitStreamReceiveWindow = quicInitStreamReceiveWindow,
                quicMaxStreamReceiveWindow = quicMaxStreamReceiveWindow,
                quicInitConnectionReceiveWindow = quicInitConnectionReceiveWindow,
                quicMaxConnectionReceiveWindow = quicMaxConnectionReceiveWindow,
                quicMaxIdleTimeout = quicMaxIdleTimeout,
                quicKeepAlivePeriod = quicKeepAlivePeriod,
                quicDisablePathMtuDiscovery = quicDisablePathMtuDiscovery,
                insecureTls = insecureTls,
            ),
        )
    }

    fun clearSavedProfileFromRust() {
        HysteriaVpnService.clearSavedProfile(applicationContext)
    }

    fun savedProfileStringFromRust(key: String): String? {
        return HysteriaVpnService.savedProfileString(applicationContext, key)
    }

    fun savedProfileBooleanFromRust(key: String, defaultValue: Boolean): Boolean {
        return HysteriaVpnService.savedProfileBoolean(applicationContext, key, defaultValue)
    }

    fun caStorageDirFromRust(): String {
        return HysteriaVpnService.caDirectory(applicationContext).absolutePath
    }

    fun caFilesListingFromRust(): String {
        return HysteriaVpnService.listCaFiles(applicationContext).joinToString("\n") {
            "${it.name}\t${it.absolutePath}"
        }
    }

    fun launchConfigImportFromRust() {
        runOnUiThread {
            startActivityForResult(
                buildOpenDocumentIntent(
                    arrayOf(
                        "application/yaml",
                        "application/x-yaml",
                        "text/yaml",
                        "text/x-yaml",
                        "text/plain",
                        "application/octet-stream",
                    )
                ),
                CONFIG_IMPORT_REQUEST_CODE,
            )
        }
    }

    fun launchCaImportFromRust() {
        runOnUiThread {
            startActivityForResult(
                buildOpenDocumentIntent(
                    arrayOf(
                        "application/x-x509-ca-cert",
                        "application/x-pem-file",
                        "application/pkix-cert",
                        "text/plain",
                        "application/octet-stream",
                    )
                ),
                CA_IMPORT_REQUEST_CODE,
            )
        }
    }

    fun consumeImportedConfigResultFromRust(): String {
        return consumePendingConfigImportResult()
    }

    fun consumeImportedCaResultFromRust(): String {
        return consumePendingCaImportResult()
    }

    private fun buildOpenDocumentIntent(mimeTypes: Array<String>): Intent {
        return Intent(Intent.ACTION_OPEN_DOCUMENT).apply {
            addCategory(Intent.CATEGORY_OPENABLE)
            type = "*/*"
            putExtra(Intent.EXTRA_MIME_TYPES, mimeTypes)
        }
    }

    private fun handleConfigImportResult(uri: Uri) {
        val name = resolveDisplayName(uri) ?: "config.yaml"
        val bytes = contentResolver.openInputStream(uri)?.use { it.readBytes() }
            ?: throw IOException("failed to read imported config")
        val content = bytes.toString(Charsets.UTF_8)
        setPendingConfigImportResult("ok${IMPORT_RESULT_DELIMITER}$name${IMPORT_RESULT_DELIMITER}$content")
        log("handleConfigImportResult name=$name bytes=${bytes.size}")
    }

    private fun handleCaImportResult(uri: Uri) {
        val sourceName = resolveDisplayName(uri) ?: "custom.crt"
        val target = uniqueCaTargetFile(sanitizeImportedFileName(sourceName))
        contentResolver.openInputStream(uri)?.use { input ->
            target.outputStream().use { output -> input.copyTo(output) }
        } ?: throw IOException("failed to read imported certificate")
        setPendingCaImportResult(
            "ok${IMPORT_RESULT_DELIMITER}${target.name}${IMPORT_RESULT_DELIMITER}${target.absolutePath}"
        )
        log("handleCaImportResult name=${target.name} path=${target.absolutePath}")
    }

    private fun resolveDisplayName(uri: Uri): String? {
        contentResolver.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME), null, null, null)
            ?.use { cursor ->
                if (cursor.moveToFirst()) {
                    return cursor.getString(0)
                }
            }
        return uri.lastPathSegment?.substringAfterLast('/')
    }

    private fun sanitizeImportedFileName(fileName: String): String {
        val sanitized = buildString(fileName.length) {
            fileName.forEach { ch ->
                when {
                    ch.isLetterOrDigit() || ch == '.' || ch == '_' || ch == '-' -> append(ch)
                    ch.isWhitespace() -> append('_')
                }
            }
        }.trim('_', '.', ' ')
        return when {
            sanitized.isEmpty() -> "imported.crt"
            sanitized.contains('.') -> sanitized
            else -> "$sanitized.crt"
        }
    }

    private fun uniqueCaTargetFile(fileName: String): File {
        val directory = HysteriaVpnService.caDirectory(applicationContext)
        val dot = fileName.lastIndexOf('.')
        val stem = if (dot >= 0) fileName.substring(0, dot) else fileName
        val extension = if (dot >= 0) fileName.substring(dot) else ""
        var candidate = File(directory, fileName)
        var index = 1
        while (candidate.exists()) {
            candidate = File(directory, "$stem-$index$extension")
            index += 1
        }
        return candidate
    }

    private fun primeAndroidActivityBridge() {
        runCatching { nativePrimeAndroidActivityBridge() }
            .onFailure { Log.e(HY_LOG_TAG, "nativePrimeAndroidActivityBridge failed", it) }
    }

    companion object {
        private const val VPN_REQUEST_CODE = 9001
        private const val CONFIG_IMPORT_REQUEST_CODE = 9002
        private const val CA_IMPORT_REQUEST_CODE = 9003
        private const val IMPORT_RESULT_DELIMITER = "\u001F"
        private val permissionGranted = AtomicBoolean(false)
        private val launchStringExtras = mutableMapOf<String, String>()
        private val launchBooleanExtras = mutableMapOf<String, Boolean>()
        private val importResultLock = Any()
        private var pendingConfigImportResult: String = ""
        private var pendingCaImportResult: String = ""
        private var instance: MainActivity? = null
        private var appContext: Context? = null

        private fun setPendingConfigImportResult(result: String) {
            synchronized(importResultLock) {
                pendingConfigImportResult = result
            }
        }

        private fun consumePendingConfigImportResult(): String {
            synchronized(importResultLock) {
                val result = pendingConfigImportResult
                pendingConfigImportResult = ""
                return result
            }
        }

        private fun setPendingCaImportResult(result: String) {
            synchronized(importResultLock) {
                pendingCaImportResult = result
            }
        }

        private fun consumePendingCaImportResult(): String {
            synchronized(importResultLock) {
                val result = pendingCaImportResult
                pendingCaImportResult = ""
                return result
            }
        }

        private fun cacheLaunchExtras(intent: Intent?) {
            if (intent == null) {
                Log.i(HY_LOG_TAG, "cacheLaunchExtras: null intent")
                return
            }
            launchStringExtras["io.hysteria.mobile.extra.SERVER"] =
                intent.getStringExtra("io.hysteria.mobile.extra.SERVER") ?: ""
            launchStringExtras["io.hysteria.mobile.extra.AUTH"] =
                intent.getStringExtra("io.hysteria.mobile.extra.AUTH") ?: ""
            launchStringExtras["io.hysteria.mobile.extra.OBFS_PASSWORD"] =
                intent.getStringExtra("io.hysteria.mobile.extra.OBFS_PASSWORD") ?: ""
            launchStringExtras["io.hysteria.mobile.extra.SNI"] =
                intent.getStringExtra("io.hysteria.mobile.extra.SNI") ?: ""
            launchStringExtras["io.hysteria.mobile.extra.CA_PATH"] =
                intent.getStringExtra("io.hysteria.mobile.extra.CA_PATH") ?: ""
            launchStringExtras["io.hysteria.mobile.extra.PIN_SHA256"] =
                intent.getStringExtra("io.hysteria.mobile.extra.PIN_SHA256") ?: ""
            launchStringExtras["io.hysteria.mobile.extra.BANDWIDTH_UP"] =
                intent.getStringExtra("io.hysteria.mobile.extra.BANDWIDTH_UP") ?: ""
            launchStringExtras["io.hysteria.mobile.extra.BANDWIDTH_DOWN"] =
                intent.getStringExtra("io.hysteria.mobile.extra.BANDWIDTH_DOWN") ?: ""
            launchStringExtras["io.hysteria.mobile.extra.QUIC_INIT_STREAM_RECEIVE_WINDOW"] =
                intent.getStringExtra("io.hysteria.mobile.extra.QUIC_INIT_STREAM_RECEIVE_WINDOW") ?: ""
            launchStringExtras["io.hysteria.mobile.extra.QUIC_MAX_STREAM_RECEIVE_WINDOW"] =
                intent.getStringExtra("io.hysteria.mobile.extra.QUIC_MAX_STREAM_RECEIVE_WINDOW") ?: ""
            launchStringExtras["io.hysteria.mobile.extra.QUIC_INIT_CONNECTION_RECEIVE_WINDOW"] =
                intent.getStringExtra("io.hysteria.mobile.extra.QUIC_INIT_CONNECTION_RECEIVE_WINDOW") ?: ""
            launchStringExtras["io.hysteria.mobile.extra.QUIC_MAX_CONNECTION_RECEIVE_WINDOW"] =
                intent.getStringExtra("io.hysteria.mobile.extra.QUIC_MAX_CONNECTION_RECEIVE_WINDOW") ?: ""
            launchStringExtras["io.hysteria.mobile.extra.QUIC_MAX_IDLE_TIMEOUT"] =
                intent.getStringExtra("io.hysteria.mobile.extra.QUIC_MAX_IDLE_TIMEOUT") ?: ""
            launchStringExtras["io.hysteria.mobile.extra.QUIC_KEEP_ALIVE_PERIOD"] =
                intent.getStringExtra("io.hysteria.mobile.extra.QUIC_KEEP_ALIVE_PERIOD") ?: ""
            launchBooleanExtras["io.hysteria.mobile.extra.INSECURE_TLS"] =
                intent.getBooleanExtra("io.hysteria.mobile.extra.INSECURE_TLS", false)
            launchBooleanExtras["io.hysteria.mobile.extra.QUIC_DISABLE_PATH_MTU_DISCOVERY"] =
                intent.getBooleanExtra("io.hysteria.mobile.extra.QUIC_DISABLE_PATH_MTU_DISCOVERY", false)
            launchBooleanExtras["io.hysteria.mobile.extra.AUTO_CONNECT"] =
                intent.getBooleanExtra("io.hysteria.mobile.extra.AUTO_CONNECT", false)
            launchBooleanExtras["io.hysteria.mobile.extra.AUTO_REQUEST_VPN"] =
                intent.getBooleanExtra("io.hysteria.mobile.extra.AUTO_REQUEST_VPN", false)
            launchBooleanExtras["io.hysteria.mobile.extra.AUTO_START_VPN"] =
                intent.getBooleanExtra("io.hysteria.mobile.extra.AUTO_START_VPN", false)
            Log.i(
                HY_LOG_TAG,
                "cacheLaunchExtras server=${launchStringExtras["io.hysteria.mobile.extra.SERVER"]} auth=${launchStringExtras["io.hysteria.mobile.extra.AUTH"]?.let { if (it.isEmpty()) "<empty>" else "<set>" }} obfs=${launchStringExtras["io.hysteria.mobile.extra.OBFS_PASSWORD"]?.let { if (it.isEmpty()) "<empty>" else "<set>" }} bandwidthUp=${launchStringExtras["io.hysteria.mobile.extra.BANDWIDTH_UP"]?.ifEmpty { "<empty>" }} bandwidthDown=${launchStringExtras["io.hysteria.mobile.extra.BANDWIDTH_DOWN"]?.ifEmpty { "<empty>" }} insecure=${launchBooleanExtras["io.hysteria.mobile.extra.INSECURE_TLS"]} pmtudOff=${launchBooleanExtras["io.hysteria.mobile.extra.QUIC_DISABLE_PATH_MTU_DISCOVERY"]} autoConnect=${launchBooleanExtras["io.hysteria.mobile.extra.AUTO_CONNECT"]} autoRequestVpn=${launchBooleanExtras["io.hysteria.mobile.extra.AUTO_REQUEST_VPN"]} autoStartVpn=${launchBooleanExtras["io.hysteria.mobile.extra.AUTO_START_VPN"]}"
            )
        }

        private fun currentIntentStringExtra(key: String): String? {
            launchStringExtras[key]?.let { cached ->
                if (cached.isNotEmpty()) {
                    return cached
                }
            }
            val current = instance ?: return null
            return current.intent?.getStringExtra(key)
        }

        private fun currentIntentBooleanExtra(key: String): Boolean {
            launchBooleanExtras[key]?.let { cached ->
                return cached
            }
            val current = instance ?: return false
            return current.intent?.getBooleanExtra(key, false) ?: false
        }

        @JvmStatic
        fun requestVpnPermissionStaticFromRust(): Boolean {
            val current = instance ?: return false
            current.requestVpnPermissionFromRust()
            return true
        }

        @JvmStatic
        fun startVpnServiceStaticFromRust(socksHost: String, socksPort: Int): Boolean {
            val current = instance
            if (current != null) {
                current.startVpnServiceFromRust(socksHost, socksPort)
                return true
            }
            val context = appContext ?: return false
            if (VpnService.prepare(context) != null) {
                return false
            }
            permissionGranted.set(true)
            HysteriaVpnService.startShellOnly(context, socksHost, socksPort)
            return true
        }

        @JvmStatic
        fun startManagedVpnStaticFromRust(
            server: String,
            auth: String,
            obfsPassword: String,
            sni: String,
            caPath: String,
            pinSha256: String,
            bandwidthUp: String,
            bandwidthDown: String,
            quicInitStreamReceiveWindow: String,
            quicMaxStreamReceiveWindow: String,
            quicInitConnectionReceiveWindow: String,
            quicMaxConnectionReceiveWindow: String,
            quicMaxIdleTimeout: String,
            quicKeepAlivePeriod: String,
            quicDisablePathMtuDiscovery: Boolean,
            insecureTls: Boolean,
            socksHost: String,
            socksPort: Int,
        ): Boolean {
            val profile = ManagedRuntimeProfile(
                server = server,
                auth = auth,
                obfsPassword = obfsPassword,
                sni = sni,
                caPath = caPath,
                pinSha256 = pinSha256,
                bandwidthUp = bandwidthUp,
                bandwidthDown = bandwidthDown,
                quicInitStreamReceiveWindow = quicInitStreamReceiveWindow,
                quicMaxStreamReceiveWindow = quicMaxStreamReceiveWindow,
                quicInitConnectionReceiveWindow = quicInitConnectionReceiveWindow,
                quicMaxConnectionReceiveWindow = quicMaxConnectionReceiveWindow,
                quicMaxIdleTimeout = quicMaxIdleTimeout,
                quicKeepAlivePeriod = quicKeepAlivePeriod,
                quicDisablePathMtuDiscovery = quicDisablePathMtuDiscovery,
                insecureTls = insecureTls,
            )
            val current = instance
            if (current != null) {
                current.startManagedVpnFromRust(profile, socksHost, socksPort)
                return true
            }
            val context = appContext ?: return false
            if (VpnService.prepare(context) != null) {
                return false
            }
            permissionGranted.set(true)
            HysteriaVpnService.startManaged(context, profile, socksHost, socksPort)
            return true
        }

        @JvmStatic
        fun stopVpnServiceStaticFromRust(): Boolean {
            val context = appContext ?: instance?.applicationContext ?: return false
            HysteriaVpnService.stopManaged(context)
            return true
        }

        @JvmStatic
        fun saveProfileStaticFromRust(
            server: String,
            auth: String,
            obfsPassword: String,
            sni: String,
            caPath: String,
            pinSha256: String,
            bandwidthUp: String,
            bandwidthDown: String,
            quicInitStreamReceiveWindow: String,
            quicMaxStreamReceiveWindow: String,
            quicInitConnectionReceiveWindow: String,
            quicMaxConnectionReceiveWindow: String,
            quicMaxIdleTimeout: String,
            quicKeepAlivePeriod: String,
            quicDisablePathMtuDiscovery: Boolean,
            insecureTls: Boolean,
        ): Boolean {
            val context = appContext ?: instance?.applicationContext ?: return false
            HysteriaVpnService.saveProfile(
                context,
                ManagedRuntimeProfile(
                    server = server,
                    auth = auth,
                    obfsPassword = obfsPassword,
                    sni = sni,
                    caPath = caPath,
                    pinSha256 = pinSha256,
                    bandwidthUp = bandwidthUp,
                    bandwidthDown = bandwidthDown,
                    quicInitStreamReceiveWindow = quicInitStreamReceiveWindow,
                    quicMaxStreamReceiveWindow = quicMaxStreamReceiveWindow,
                    quicInitConnectionReceiveWindow = quicInitConnectionReceiveWindow,
                    quicMaxConnectionReceiveWindow = quicMaxConnectionReceiveWindow,
                    quicMaxIdleTimeout = quicMaxIdleTimeout,
                    quicKeepAlivePeriod = quicKeepAlivePeriod,
                    quicDisablePathMtuDiscovery = quicDisablePathMtuDiscovery,
                    insecureTls = insecureTls,
                ),
            )
            Log.i(
                HY_LOG_TAG,
                "saveProfileStaticFromRust server=$server auth=${if (auth.isEmpty()) "<empty>" else "<set>"} obfs=${if (obfsPassword.isEmpty()) "<empty>" else "<set>"} insecure=$insecureTls"
            )
            return true
        }

        @JvmStatic
        fun clearSavedProfileStaticFromRust(): Boolean {
            val context = appContext ?: instance?.applicationContext ?: return false
            HysteriaVpnService.clearSavedProfile(context)
            Log.i(HY_LOG_TAG, "clearSavedProfileStaticFromRust")
            return true
        }

        @JvmStatic
        fun savedProfileStringStaticFromRust(key: String): String? {
            val context = appContext ?: instance?.applicationContext ?: return null
            return HysteriaVpnService.savedProfileString(context, key)
        }

        @JvmStatic
        fun savedProfileBooleanStaticFromRust(key: String, defaultValue: Boolean): Boolean {
            val context = appContext ?: instance?.applicationContext ?: return defaultValue
            return HysteriaVpnService.savedProfileBoolean(context, key, defaultValue)
        }

        @JvmStatic
        fun isVpnPermissionGrantedStaticFromRust(): Boolean {
            val current = instance
            val context = appContext
            return permissionGranted.get()
                || (current != null && VpnService.prepare(current) == null)
                || (context != null && VpnService.prepare(context) == null)
        }

        @JvmStatic
        fun isVpnActiveStaticFromRust(): Boolean {
            return HysteriaVpnService.isActive()
        }

        @JvmStatic
        fun launchStringExtraStaticFromRust(key: String): String? {
            val value = currentIntentStringExtra(key)
            Log.i(HY_LOG_TAG, "launchStringExtraStaticFromRust key=$key value=${if (value.isNullOrEmpty()) "<empty>" else value}")
            return value
        }

        @JvmStatic
        fun launchBooleanExtraStaticFromRust(key: String): Boolean {
            val value = currentIntentBooleanExtra(key)
            Log.i(HY_LOG_TAG, "launchBooleanExtraStaticFromRust key=$key value=$value")
            return value
        }

        @JvmStatic
        fun protectVpnFdStaticFromRust(fd: Int): Boolean {
            return HysteriaVpnService.protectFdFromRust(fd)
        }

        @JvmStatic
        fun takeVpnTunFdStaticFromRust(): Int {
            return HysteriaVpnService.dupTunFdForRust()
        }

        @JvmStatic
        external fun nativePrimeAndroidBridge()
    }

    external fun nativePrimeAndroidActivityBridge()
}

class HysteriaVpnService : VpnService() {
    private var vpnInterface: ParcelFileDescriptor? = null

    override fun onCreate() {
        super.onCreate()
        appContext = applicationContext
        instance = this
        registerAppContext(applicationContext)
        runCatching { nativePrimeAndroidServiceBridge() }
            .onFailure { Log.e(HY_LOG_TAG, "nativePrimeAndroidServiceBridge failed in service onCreate", it) }
        createNotificationChannel()
    }

    override fun onDestroy() {
        runCatching { nativeStopManagedRuntime() }
            .onFailure { Log.w(HY_LOG_TAG, "nativeStopManagedRuntime failed in onDestroy", it) }
        shutdownVpn()
        if (instance === this) {
            instance = null
        }
        super.onDestroy()
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        Log.i(HY_LOG_TAG, "HysteriaVpnService onStartCommand action=${intent?.action} extras=${intent?.extras}")
        when (intent?.action) {
            ACTION_STOP -> {
                clearManagedRecord(this)
                shutdownVpn()
                stopForeground(STOP_FOREGROUND_REMOVE)
                stopSelf()
                return START_NOT_STICKY
            }

            else -> {
                val record = loadManagedRecord(this)
                val socksHost = intent?.getStringExtra(EXTRA_SOCKS_HOST)
                    ?: record?.socksHost
                    ?: "127.0.0.1"
                val socksPort = intent?.getIntExtra(EXTRA_SOCKS_PORT, -1)
                    ?.takeIf { it > 0 }
                    ?: record?.socksPort
                    ?: 1080

                startForeground(NOTIFICATION_ID, buildNotification(socksHost, socksPort))
                startVpnShell(socksHost, socksPort)

                record?.let {
                    runCatching {
                        nativeStartManagedRuntime(
                            it.profile.server,
                            it.profile.auth,
                            it.profile.obfsPassword,
                            it.profile.sni,
                            it.profile.caPath,
                            it.profile.pinSha256,
                            it.profile.bandwidthUp,
                            it.profile.bandwidthDown,
                            it.profile.quicInitStreamReceiveWindow,
                            it.profile.quicMaxStreamReceiveWindow,
                            it.profile.quicInitConnectionReceiveWindow,
                            it.profile.quicMaxConnectionReceiveWindow,
                            it.profile.quicMaxIdleTimeout,
                            it.profile.quicKeepAlivePeriod,
                            it.profile.quicDisablePathMtuDiscovery,
                            it.profile.insecureTls,
                            true,
                        )
                    }.onFailure { err ->
                        Log.e(HY_LOG_TAG, "nativeStartManagedRuntime failed", err)
                    }
                }

                return START_STICKY
            }
        }
    }

    private fun startVpnShell(socksHost: String, socksPort: Int) {
        Log.i(HY_LOG_TAG, "startVpnShell socks=$socksHost:$socksPort")
        if (vpnInterface != null) {
            currentSocksEndpoint = "$socksHost:$socksPort"
            return
        }

        val builder = Builder()
            .setSession("Hysteria")
            .setMtu(1500)
            .addAddress("10.8.0.2", 32)
            .addDnsServer("1.1.1.1")
            .addDnsServer("8.8.8.8")
            .addRoute("0.0.0.0", 0)

        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            builder.addAddress("fd00::2", 128)
            builder.addRoute("::", 0)
        }

        vpnInterface = builder.establish()
        active.set(vpnInterface != null)
        Log.i(HY_LOG_TAG, "startVpnShell established=${vpnInterface != null}")
        currentSocksEndpoint = "$socksHost:$socksPort"
    }

    private fun shutdownVpn() {
        Log.i(HY_LOG_TAG, "shutdownVpn")
        active.set(false)
        currentSocksEndpoint = null
        vpnInterface?.close()
        vpnInterface = null
    }

    private fun buildNotification(socksHost: String, socksPort: Int): Notification {
        return NotificationCompat.Builder(this, NOTIFICATION_CHANNEL)
            .setContentTitle("Hysteria VPN")
            .setContentText("Managed runtime active. Local SOCKS $socksHost:$socksPort")
            .setSmallIcon(android.R.drawable.stat_sys_download_done)
            .setOngoing(true)
            .build()
    }

    fun stopManagedFromRust() {
        stopManaged(applicationContext)
    }

    fun isPermissionGrantedFromRust(): Boolean {
        return active.get() || VpnService.prepare(applicationContext) == null
    }

    fun isActiveFromRust(): Boolean {
        return active.get()
    }

    fun takeTunFdFromRust(): Int {
        val vpn = vpnInterface ?: return -1
        return try {
            ParcelFileDescriptor.dup(vpn.fileDescriptor).detachFd()
        } catch (_: IOException) {
            -1
        }
    }

    fun protectManagedFdFromRust(fd: Int): Boolean {
        return protect(fd)
    }

    external fun nativePrimeAndroidServiceBridge()

    private fun createNotificationChannel() {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            val manager = getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
            val channel = NotificationChannel(
                NOTIFICATION_CHANNEL,
                "Hysteria VPN",
                NotificationManager.IMPORTANCE_LOW,
            )
            manager.createNotificationChannel(channel)
        }
    }

    companion object {
        const val ACTION_START = "io.hysteria.mobile.action.START_VPN"
        const val ACTION_STOP = "io.hysteria.mobile.action.STOP_VPN"
        const val EXTRA_SOCKS_HOST = "io.hysteria.mobile.extra.SOCKS_HOST"
        const val EXTRA_SOCKS_PORT = "io.hysteria.mobile.extra.SOCKS_PORT"
        private const val NOTIFICATION_CHANNEL = "hysteria_vpn"
        private const val NOTIFICATION_ID = 7001
        private const val PREFS_NAME = "hysteria_mobile"
        private const val KEY_PROFILE_SERVER = "profile.server"
        private const val KEY_PROFILE_AUTH = "profile.auth"
        private const val KEY_PROFILE_OBFS = "profile.obfs"
        private const val KEY_PROFILE_SNI = "profile.sni"
        private const val KEY_PROFILE_CA_PATH = "profile.ca_path"
        private const val KEY_PROFILE_PIN_SHA256 = "profile.pin_sha256"
        private const val KEY_PROFILE_BANDWIDTH_UP = "profile.bandwidth.up"
        private const val KEY_PROFILE_BANDWIDTH_DOWN = "profile.bandwidth.down"
        private const val KEY_PROFILE_QUIC_INIT_STREAM_RECEIVE_WINDOW =
            "profile.quic.init_stream_receive_window"
        private const val KEY_PROFILE_QUIC_MAX_STREAM_RECEIVE_WINDOW =
            "profile.quic.max_stream_receive_window"
        private const val KEY_PROFILE_QUIC_INIT_CONNECTION_RECEIVE_WINDOW =
            "profile.quic.init_connection_receive_window"
        private const val KEY_PROFILE_QUIC_MAX_CONNECTION_RECEIVE_WINDOW =
            "profile.quic.max_connection_receive_window"
        private const val KEY_PROFILE_QUIC_MAX_IDLE_TIMEOUT = "profile.quic.max_idle_timeout"
        private const val KEY_PROFILE_QUIC_KEEP_ALIVE_PERIOD = "profile.quic.keep_alive_period"
        private const val KEY_PROFILE_QUIC_DISABLE_PATH_MTU_DISCOVERY =
            "profile.quic.disable_path_mtu_discovery"
        private const val KEY_PROFILE_INSECURE_TLS = "profile.insecure_tls"
        private const val KEY_SERVER = "managed.server"
        private const val KEY_AUTH = "managed.auth"
        private const val KEY_OBFS = "managed.obfs"
        private const val KEY_SNI = "managed.sni"
        private const val KEY_CA_PATH = "managed.ca_path"
        private const val KEY_PIN_SHA256 = "managed.pin_sha256"
        private const val KEY_BANDWIDTH_UP = "managed.bandwidth.up"
        private const val KEY_BANDWIDTH_DOWN = "managed.bandwidth.down"
        private const val KEY_QUIC_INIT_STREAM_RECEIVE_WINDOW =
            "managed.quic.init_stream_receive_window"
        private const val KEY_QUIC_MAX_STREAM_RECEIVE_WINDOW =
            "managed.quic.max_stream_receive_window"
        private const val KEY_QUIC_INIT_CONNECTION_RECEIVE_WINDOW =
            "managed.quic.init_connection_receive_window"
        private const val KEY_QUIC_MAX_CONNECTION_RECEIVE_WINDOW =
            "managed.quic.max_connection_receive_window"
        private const val KEY_QUIC_MAX_IDLE_TIMEOUT = "managed.quic.max_idle_timeout"
        private const val KEY_QUIC_KEEP_ALIVE_PERIOD = "managed.quic.keep_alive_period"
        private const val KEY_QUIC_DISABLE_PATH_MTU_DISCOVERY =
            "managed.quic.disable_path_mtu_discovery"
        private const val KEY_INSECURE_TLS = "managed.insecure_tls"
        private const val KEY_SOCKS_HOST = "managed.socks_host"
        private const val KEY_SOCKS_PORT = "managed.socks_port"

        private val active = AtomicBoolean(false)
        private var currentSocksEndpoint: String? = null
        private var instance: HysteriaVpnService? = null
        private var appContext: Context? = null

        init {
            runCatching { System.loadLibrary("dioxusmain") }
                .onFailure { Log.w(HY_LOG_TAG, "System.loadLibrary(dioxusmain) failed", it) }
        }

        @JvmStatic
        external fun nativeStartManagedRuntime(
            server: String,
            auth: String,
            obfsPassword: String,
            sni: String,
            caPath: String,
            pinSha256: String,
            bandwidthUp: String,
            bandwidthDown: String,
            quicInitStreamReceiveWindow: String,
            quicMaxStreamReceiveWindow: String,
            quicInitConnectionReceiveWindow: String,
            quicMaxConnectionReceiveWindow: String,
            quicMaxIdleTimeout: String,
            quicKeepAlivePeriod: String,
            quicDisablePathMtuDiscovery: Boolean,
            insecureTls: Boolean,
            restoreVpn: Boolean,
        )

        @JvmStatic
        external fun nativeStopManagedRuntime()

        private fun prefs(context: Context): SharedPreferences {
            return context.applicationContext.getSharedPreferences(PREFS_NAME, Context.MODE_PRIVATE)
        }

        fun caDirectory(context: Context): File {
            val app = context.applicationContext
            val root = app.getExternalFilesDir(null) ?: app.filesDir
            return root.resolve("certs").apply {
                if (!exists()) {
                    mkdirs()
                }
            }
        }

        fun listCaFiles(context: Context): List<File> {
            return caDirectory(context)
                .listFiles()
                ?.asSequence()
                ?.filter { it.isFile }
                ?.sortedBy { it.name.lowercase() }
                ?.toList()
                ?: emptyList()
        }

        fun saveProfile(context: Context, profile: ManagedRuntimeProfile) {
            prefs(context).edit()
                .putString(KEY_PROFILE_SERVER, profile.server)
                .putString(KEY_PROFILE_AUTH, profile.auth)
                .putString(KEY_PROFILE_OBFS, profile.obfsPassword)
                .putString(KEY_PROFILE_SNI, profile.sni)
                .putString(KEY_PROFILE_CA_PATH, profile.caPath)
                .putString(KEY_PROFILE_PIN_SHA256, profile.pinSha256)
                .putString(KEY_PROFILE_BANDWIDTH_UP, profile.bandwidthUp)
                .putString(KEY_PROFILE_BANDWIDTH_DOWN, profile.bandwidthDown)
                .putString(
                    KEY_PROFILE_QUIC_INIT_STREAM_RECEIVE_WINDOW,
                    profile.quicInitStreamReceiveWindow,
                )
                .putString(
                    KEY_PROFILE_QUIC_MAX_STREAM_RECEIVE_WINDOW,
                    profile.quicMaxStreamReceiveWindow,
                )
                .putString(
                    KEY_PROFILE_QUIC_INIT_CONNECTION_RECEIVE_WINDOW,
                    profile.quicInitConnectionReceiveWindow,
                )
                .putString(
                    KEY_PROFILE_QUIC_MAX_CONNECTION_RECEIVE_WINDOW,
                    profile.quicMaxConnectionReceiveWindow,
                )
                .putString(KEY_PROFILE_QUIC_MAX_IDLE_TIMEOUT, profile.quicMaxIdleTimeout)
                .putString(KEY_PROFILE_QUIC_KEEP_ALIVE_PERIOD, profile.quicKeepAlivePeriod)
                .putBoolean(
                    KEY_PROFILE_QUIC_DISABLE_PATH_MTU_DISCOVERY,
                    profile.quicDisablePathMtuDiscovery,
                )
                .putBoolean(KEY_PROFILE_INSECURE_TLS, profile.insecureTls)
                .apply()
        }

        fun clearSavedProfile(context: Context) {
            prefs(context).edit()
                .remove(KEY_PROFILE_SERVER)
                .remove(KEY_PROFILE_AUTH)
                .remove(KEY_PROFILE_OBFS)
                .remove(KEY_PROFILE_SNI)
                .remove(KEY_PROFILE_CA_PATH)
                .remove(KEY_PROFILE_PIN_SHA256)
                .remove(KEY_PROFILE_BANDWIDTH_UP)
                .remove(KEY_PROFILE_BANDWIDTH_DOWN)
                .remove(KEY_PROFILE_QUIC_INIT_STREAM_RECEIVE_WINDOW)
                .remove(KEY_PROFILE_QUIC_MAX_STREAM_RECEIVE_WINDOW)
                .remove(KEY_PROFILE_QUIC_INIT_CONNECTION_RECEIVE_WINDOW)
                .remove(KEY_PROFILE_QUIC_MAX_CONNECTION_RECEIVE_WINDOW)
                .remove(KEY_PROFILE_QUIC_MAX_IDLE_TIMEOUT)
                .remove(KEY_PROFILE_QUIC_KEEP_ALIVE_PERIOD)
                .remove(KEY_PROFILE_QUIC_DISABLE_PATH_MTU_DISCOVERY)
                .remove(KEY_PROFILE_INSECURE_TLS)
                .apply()
        }

        fun savedProfileString(context: Context, key: String): String? {
            return prefs(context).getString(key, null)
        }

        fun savedProfileBoolean(context: Context, key: String, defaultValue: Boolean): Boolean {
            return prefs(context).getBoolean(key, defaultValue)
        }

        private fun saveManagedRecord(context: Context, record: ManagedRuntimeRecord) {
            prefs(context).edit()
                .putString(KEY_SERVER, record.profile.server)
                .putString(KEY_AUTH, record.profile.auth)
                .putString(KEY_OBFS, record.profile.obfsPassword)
                .putString(KEY_SNI, record.profile.sni)
                .putString(KEY_CA_PATH, record.profile.caPath)
                .putString(KEY_PIN_SHA256, record.profile.pinSha256)
                .putString(KEY_BANDWIDTH_UP, record.profile.bandwidthUp)
                .putString(KEY_BANDWIDTH_DOWN, record.profile.bandwidthDown)
                .putString(
                    KEY_QUIC_INIT_STREAM_RECEIVE_WINDOW,
                    record.profile.quicInitStreamReceiveWindow,
                )
                .putString(
                    KEY_QUIC_MAX_STREAM_RECEIVE_WINDOW,
                    record.profile.quicMaxStreamReceiveWindow,
                )
                .putString(
                    KEY_QUIC_INIT_CONNECTION_RECEIVE_WINDOW,
                    record.profile.quicInitConnectionReceiveWindow,
                )
                .putString(
                    KEY_QUIC_MAX_CONNECTION_RECEIVE_WINDOW,
                    record.profile.quicMaxConnectionReceiveWindow,
                )
                .putString(KEY_QUIC_MAX_IDLE_TIMEOUT, record.profile.quicMaxIdleTimeout)
                .putString(KEY_QUIC_KEEP_ALIVE_PERIOD, record.profile.quicKeepAlivePeriod)
                .putBoolean(
                    KEY_QUIC_DISABLE_PATH_MTU_DISCOVERY,
                    record.profile.quicDisablePathMtuDiscovery,
                )
                .putBoolean(KEY_INSECURE_TLS, record.profile.insecureTls)
                .putString(KEY_SOCKS_HOST, record.socksHost)
                .putInt(KEY_SOCKS_PORT, record.socksPort)
                .apply()
        }

        private fun clearManagedRecord(context: Context) {
            prefs(context).edit()
                .remove(KEY_SERVER)
                .remove(KEY_AUTH)
                .remove(KEY_OBFS)
                .remove(KEY_SNI)
                .remove(KEY_CA_PATH)
                .remove(KEY_PIN_SHA256)
                .remove(KEY_BANDWIDTH_UP)
                .remove(KEY_BANDWIDTH_DOWN)
                .remove(KEY_QUIC_INIT_STREAM_RECEIVE_WINDOW)
                .remove(KEY_QUIC_MAX_STREAM_RECEIVE_WINDOW)
                .remove(KEY_QUIC_INIT_CONNECTION_RECEIVE_WINDOW)
                .remove(KEY_QUIC_MAX_CONNECTION_RECEIVE_WINDOW)
                .remove(KEY_QUIC_MAX_IDLE_TIMEOUT)
                .remove(KEY_QUIC_KEEP_ALIVE_PERIOD)
                .remove(KEY_QUIC_DISABLE_PATH_MTU_DISCOVERY)
                .remove(KEY_INSECURE_TLS)
                .remove(KEY_SOCKS_HOST)
                .remove(KEY_SOCKS_PORT)
                .apply()
        }

        private fun loadManagedRecord(context: Context): ManagedRuntimeRecord? {
            val prefs = prefs(context)
            val server = prefs.getString(KEY_SERVER, null) ?: return null
            val auth = prefs.getString(KEY_AUTH, null) ?: return null
            val socksHost = prefs.getString(KEY_SOCKS_HOST, null) ?: "127.0.0.1"
            val socksPort = prefs.getInt(KEY_SOCKS_PORT, 1080)
            return ManagedRuntimeRecord(
                profile = ManagedRuntimeProfile(
                    server = server,
                    auth = auth,
                    obfsPassword = prefs.getString(KEY_OBFS, "") ?: "",
                    sni = prefs.getString(KEY_SNI, "") ?: "",
                    caPath = prefs.getString(KEY_CA_PATH, "") ?: "",
                    pinSha256 = prefs.getString(KEY_PIN_SHA256, "") ?: "",
                    bandwidthUp = prefs.getString(KEY_BANDWIDTH_UP, "") ?: "",
                    bandwidthDown = prefs.getString(KEY_BANDWIDTH_DOWN, "") ?: "",
                    quicInitStreamReceiveWindow =
                        prefs.getString(KEY_QUIC_INIT_STREAM_RECEIVE_WINDOW, "") ?: "",
                    quicMaxStreamReceiveWindow =
                        prefs.getString(KEY_QUIC_MAX_STREAM_RECEIVE_WINDOW, "") ?: "",
                    quicInitConnectionReceiveWindow =
                        prefs.getString(KEY_QUIC_INIT_CONNECTION_RECEIVE_WINDOW, "") ?: "",
                    quicMaxConnectionReceiveWindow =
                        prefs.getString(KEY_QUIC_MAX_CONNECTION_RECEIVE_WINDOW, "") ?: "",
                    quicMaxIdleTimeout = prefs.getString(KEY_QUIC_MAX_IDLE_TIMEOUT, "") ?: "",
                    quicKeepAlivePeriod =
                        prefs.getString(KEY_QUIC_KEEP_ALIVE_PERIOD, "") ?: "",
                    quicDisablePathMtuDiscovery =
                        prefs.getBoolean(KEY_QUIC_DISABLE_PATH_MTU_DISCOVERY, false),
                    insecureTls = prefs.getBoolean(KEY_INSECURE_TLS, false),
                ),
                socksHost = socksHost,
                socksPort = socksPort,
            )
        }

        private fun launchServiceIntent(context: Context, intent: Intent) {
            val app = context.applicationContext
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                app.startForegroundService(intent)
            } else {
                app.startService(intent)
            }
        }

        fun startShellOnly(context: Context, socksHost: String, socksPort: Int) {
            clearManagedRecord(context)
            val intent = Intent(context, HysteriaVpnService::class.java).apply {
                action = ACTION_START
                putExtra(EXTRA_SOCKS_HOST, socksHost)
                putExtra(EXTRA_SOCKS_PORT, socksPort)
            }
            launchServiceIntent(context, intent)
        }

        fun startManaged(
            context: Context,
            profile: ManagedRuntimeProfile,
            socksHost: String,
            socksPort: Int,
        ) {
            saveManagedRecord(context, ManagedRuntimeRecord(profile, socksHost, socksPort))
            val intent = Intent(context, HysteriaVpnService::class.java).apply {
                action = ACTION_START
                putExtra(EXTRA_SOCKS_HOST, socksHost)
                putExtra(EXTRA_SOCKS_PORT, socksPort)
            }
            launchServiceIntent(context, intent)
        }

        fun stopManaged(context: Context) {
            clearManagedRecord(context)
            val app = context.applicationContext
            app.stopService(Intent(app, HysteriaVpnService::class.java))
        }

        @JvmStatic
        fun protectFdFromRust(fd: Int): Boolean {
            return instance?.protect(fd) ?: false
        }

        @JvmStatic
        fun isActive(): Boolean {
            return active.get()
        }

        @JvmStatic
        fun isPermissionGrantedStaticFromRust(): Boolean {
            val context = appContext
            return active.get() || (context != null && VpnService.prepare(context) == null)
        }

        @JvmStatic
        fun stopManagedStaticFromRust(): Boolean {
            val context = appContext ?: return false
            stopManaged(context)
            return true
        }

        @JvmStatic
        fun saveProfileStaticFromRust(
            server: String,
            auth: String,
            obfsPassword: String,
            sni: String,
            caPath: String,
            pinSha256: String,
            bandwidthUp: String,
            bandwidthDown: String,
            quicInitStreamReceiveWindow: String,
            quicMaxStreamReceiveWindow: String,
            quicInitConnectionReceiveWindow: String,
            quicMaxConnectionReceiveWindow: String,
            quicMaxIdleTimeout: String,
            quicKeepAlivePeriod: String,
            quicDisablePathMtuDiscovery: Boolean,
            insecureTls: Boolean,
        ): Boolean {
            val context = appContext ?: return false
            saveProfile(
                context,
                ManagedRuntimeProfile(
                    server = server,
                    auth = auth,
                    obfsPassword = obfsPassword,
                    sni = sni,
                    caPath = caPath,
                    pinSha256 = pinSha256,
                    bandwidthUp = bandwidthUp,
                    bandwidthDown = bandwidthDown,
                    quicInitStreamReceiveWindow = quicInitStreamReceiveWindow,
                    quicMaxStreamReceiveWindow = quicMaxStreamReceiveWindow,
                    quicInitConnectionReceiveWindow = quicInitConnectionReceiveWindow,
                    quicMaxConnectionReceiveWindow = quicMaxConnectionReceiveWindow,
                    quicMaxIdleTimeout = quicMaxIdleTimeout,
                    quicKeepAlivePeriod = quicKeepAlivePeriod,
                    quicDisablePathMtuDiscovery = quicDisablePathMtuDiscovery,
                    insecureTls = insecureTls,
                ),
            )
            return true
        }

        @JvmStatic
        fun clearSavedProfileStaticFromRust(): Boolean {
            val context = appContext ?: return false
            clearSavedProfile(context)
            return true
        }

        @JvmStatic
        fun savedProfileStringStaticFromRust(key: String): String? {
            val context = appContext ?: return null
            return savedProfileString(context, key)
        }

        @JvmStatic
        fun savedProfileBooleanStaticFromRust(key: String, defaultValue: Boolean): Boolean {
            val context = appContext ?: return defaultValue
            return savedProfileBoolean(context, key, defaultValue)
        }

        @JvmStatic
        fun dupTunFdForRust(): Int {
            val service = instance ?: return -1
            val vpn = service.vpnInterface ?: return -1
            return try {
                ParcelFileDescriptor.dup(vpn.fileDescriptor).detachFd()
            } catch (_: IOException) {
                -1
            }
        }

        @JvmStatic
        fun socksEndpoint(): String? {
            return currentSocksEndpoint
        }

        fun registerAppContext(context: Context) {
            appContext = context.applicationContext
        }
    }
}
