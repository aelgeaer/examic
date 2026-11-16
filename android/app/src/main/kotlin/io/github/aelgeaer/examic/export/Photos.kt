package io.github.aelgeaer.examic.export

import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.graphics.Matrix
import androidx.exifinterface.media.ExifInterface
import java.io.File

/**
 * Decodes a photo no larger than [maxPixels] on its long side, turned upright.
 *
 * CameraX records the camera's orientation in the JPEG's EXIF data instead of
 * rotating the pixels, so it is applied here.
 */
fun decodePhoto(file: File, maxPixels: Int): Bitmap? {
    if (!file.isFile) return null
    val bounds = BitmapFactory.Options().apply { inJustDecodeBounds = true }
    BitmapFactory.decodeFile(file.path, bounds)
    if (bounds.outWidth <= 0 || bounds.outHeight <= 0) return null
    var sample = 1
    while (maxOf(bounds.outWidth, bounds.outHeight) / (sample * 2) >= maxPixels) sample *= 2
    val bitmap = BitmapFactory.decodeFile(file.path, BitmapFactory.Options().apply { inSampleSize = sample })
        ?: return null
    val degrees = runCatching { ExifInterface(file.path).rotationDegrees }.getOrDefault(0)
    if (degrees == 0) return bitmap
    val rotated = Bitmap.createBitmap(
        bitmap, 0, 0, bitmap.width, bitmap.height, Matrix().apply { postRotate(degrees.toFloat()) }, true,
    )
    if (rotated !== bitmap) bitmap.recycle()
    return rotated
}
