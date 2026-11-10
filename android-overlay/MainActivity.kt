package com.mans.iloaderandroid

import android.app.PendingIntent
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.hardware.usb.UsbDevice
import android.hardware.usb.UsbDeviceConnection
import android.hardware.usb.UsbManager
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.util.Log
import androidx.core.content.ContextCompat

/**
 * Hosts the Tauri web view and owns Android's side of the USB connection to the iPhone:
 * finding the device, asking the user for permission, opening it and handing the file
 * descriptor to the Rust usbmux implementation (src-tauri/src/android_usbmuxd.rs).
 */
class MainActivity : TauriActivity() {
  companion object {
    private const val TAG = "iloaderUsb"
    private const val APPLE_VENDOR_ID = 0x05AC
    private const val POLL_INTERVAL_MS = 2000L
    private const val RETRY_AFTER_FAILURE_MS = 6000L

    // Values of nativeUsbState(), mirrored from android_usbmuxd.rs.
    private const val STATE_FAILED = 3

    private var nativeLoaded = false

    init {
      try {
        System.loadLibrary("iloader_lib")
        nativeLoaded = true
      } catch (e: UnsatisfiedLinkError) {
        Log.e(TAG, "Could not load native library", e)
      }
    }
  }

  private external fun nativeUsbAttached(fd: Int, serial: String, productId: Int)
  private external fun nativeUsbDetached()
  private external fun nativeUsbState(): Int

  private lateinit var usbManager: UsbManager
  private val handler = Handler(Looper.getMainLooper())
  private var connection: UsbDeviceConnection? = null
  private var connectedDeviceName: String? = null
  private var openedAt = 0L
  private var permissionRequestedFor: String? = null
  private var resumed = false

  private val permissionAction: String
    get() = "$packageName.USB_PERMISSION"

  private val usbReceiver = object : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
      when (intent.action) {
        UsbManager.ACTION_USB_DEVICE_DETACHED -> {
          val device = deviceFrom(intent)
          if (device == null || device.deviceName == connectedDeviceName) {
            closeUsb()
          }
          if (device?.deviceName == permissionRequestedFor) {
            permissionRequestedFor = null
          }
        }
        else -> scanUsb()
      }
    }
  }

  private val poll = object : Runnable {
    override fun run() {
      scanUsb()
      if (resumed) {
        handler.postDelayed(this, POLL_INTERVAL_MS)
      }
    }
  }

  override fun onCreate(savedInstanceState: Bundle?) {
    super.onCreate(savedInstanceState)
    usbManager = getSystemService(Context.USB_SERVICE) as UsbManager

    val filter = IntentFilter().apply {
      addAction(permissionAction)
      addAction(UsbManager.ACTION_USB_DEVICE_ATTACHED)
      addAction(UsbManager.ACTION_USB_DEVICE_DETACHED)
    }
    ContextCompat.registerReceiver(this, usbReceiver, filter, ContextCompat.RECEIVER_NOT_EXPORTED)
  }

  override fun onNewIntent(intent: Intent) {
    super.onNewIntent(intent)
    setIntent(intent)
    scanUsb()
  }

  override fun onResume() {
    super.onResume()
    resumed = true
    handler.removeCallbacks(poll)
    handler.post(poll)
  }

  override fun onPause() {
    super.onPause()
    resumed = false
    handler.removeCallbacks(poll)
  }

  override fun onDestroy() {
    handler.removeCallbacks(poll)
    try {
      unregisterReceiver(usbReceiver)
    } catch (e: IllegalArgumentException) {
      // Was never registered.
    }
    closeUsb()
    super.onDestroy()
  }

  @Suppress("DEPRECATION")
  private fun deviceFrom(intent: Intent): UsbDevice? =
    if (Build.VERSION.SDK_INT >= 33) {
      intent.getParcelableExtra(UsbManager.EXTRA_DEVICE, UsbDevice::class.java)
    } else {
      intent.getParcelableExtra(UsbManager.EXTRA_DEVICE)
    }

  /** Looks for an attached Apple device and connects to it, asking for permission if needed. */
  private fun scanUsb() {
    if (!nativeLoaded) return

    val devices = try {
      usbManager.deviceList.values.filter { it.vendorId == APPLE_VENDOR_ID }
    } catch (e: Exception) {
      Log.w(TAG, "Could not list USB devices", e)
      return
    }

    if (connection != null) {
      val stillAttached = devices.any { it.deviceName == connectedDeviceName }
      val failed = nativeUsbState() == STATE_FAILED &&
        SystemClock.elapsedRealtime() - openedAt > RETRY_AFTER_FAILURE_MS
      if (stillAttached && !failed) return
      // Unplugged, or the native side gave up on it: start over.
      closeUsb()
      if (!stillAttached) return
    }

    val device = devices.firstOrNull() ?: return
    if (!usbManager.hasPermission(device)) {
      if (permissionRequestedFor != device.deviceName) {
        permissionRequestedFor = device.deviceName
        val flags = if (Build.VERSION.SDK_INT >= 31) PendingIntent.FLAG_MUTABLE else 0
        val intent = Intent(permissionAction).setPackage(packageName)
        usbManager.requestPermission(device, PendingIntent.getBroadcast(this, 0, intent, flags))
      }
      return
    }
    permissionRequestedFor = null
    openUsb(device)
  }

  private fun openUsb(device: UsbDevice) {
    val conn: UsbDeviceConnection? = try {
      usbManager.openDevice(device)
    } catch (e: Exception) {
      Log.e(TAG, "Could not open USB device", e)
      null
    }
    if (conn == null) return

    val serial = try {
      device.serialNumber ?: ""
    } catch (e: SecurityException) {
      ""
    }

    connection = conn
    connectedDeviceName = device.deviceName
    openedAt = SystemClock.elapsedRealtime()
    try {
      nativeUsbAttached(conn.fileDescriptor, serial, device.productId)
    } catch (e: UnsatisfiedLinkError) {
      Log.e(TAG, "Native USB entry point missing", e)
      conn.close()
      connection = null
      connectedDeviceName = null
    }
  }

  private fun closeUsb() {
    if (connection == null) return
    try {
      nativeUsbDetached()
    } catch (e: UnsatisfiedLinkError) {
      Log.e(TAG, "Native USB entry point missing", e)
    }
    connection?.close()
    connection = null
    connectedDeviceName = null
  }
}
