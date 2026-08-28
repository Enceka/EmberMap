package net.yeah.enceka.embermap

import android.os.Bundle
import android.util.Log
import androidx.activity.enableEdgeToEdge
import java.io.File

/**
 * Tauri 在 Android 上把 resource_dir() 解析为 asset:// URI，Rust 的 std::fs 读不了；
 * 因此启动时把 APK assets 里的数据包（掩码/手绘图/元数据）解压到 dataDir/bundle，
 * Rust 侧 resolve_bundle_dir 从 app_data_dir()/bundle 读取。
 *
 * 用 bundle.json 的大小做版本标记：与 assets 中不一致才重新解压，避免每次启动都拷贝。
 */
class MainActivity : TauriActivity() {
  override fun onCreate(savedInstanceState: Bundle?) {
    enableEdgeToEdge()
    extractBundle()
    super.onCreate(savedInstanceState)
  }

  private fun extractBundle() {
    try {
      val target = File(dataDir, "bundle")
      val stamp = File(target, ".asset-size")
      val current = assets.open("bundle/bundle.json").use { it.available() }.toString()
      if (stamp.isFile && stamp.readText() == current) return

      target.deleteRecursively()
      copyAssetDir("bundle", target)
      stamp.writeText(current)
      Log.i("EmberMap", "数据包已解压到 ${target.absolutePath}")
    } catch (e: Exception) {
      Log.e("EmberMap", "数据包解压失败", e)
    }
  }

  private fun copyAssetDir(path: String, dest: File) {
    val children = assets.list(path) ?: return
    if (children.isEmpty()) {
      dest.parentFile?.mkdirs()
      assets.open(path).use { input -> dest.outputStream().use { input.copyTo(it) } }
      return
    }
    dest.mkdirs()
    for (child in children) {
      copyAssetDir("$path/$child", File(dest, child))
    }
  }
}
