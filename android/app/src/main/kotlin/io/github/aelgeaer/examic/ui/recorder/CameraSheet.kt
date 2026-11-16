package io.github.aelgeaer.examic.ui.recorder

import android.Manifest
import android.content.pm.PackageManager
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.camera.core.CameraSelector
import androidx.camera.core.ImageCapture
import androidx.camera.core.ImageCaptureException
import androidx.camera.core.Preview
import androidx.camera.lifecycle.ProcessCameraProvider
import androidx.camera.view.PreviewView
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.aspectRatio
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.FilledIconButton
import androidx.compose.material3.FilledTonalIconButton
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.ModalBottomSheet
import androidx.compose.material3.Text
import androidx.compose.material3.rememberModalBottomSheetState
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.unit.dp
import androidx.compose.ui.viewinterop.AndroidView
import androidx.core.content.ContextCompat
import androidx.lifecycle.compose.LocalLifecycleOwner
import io.github.aelgeaer.examic.R
import io.github.aelgeaer.examic.audio.NativeAudio
import java.io.File
import java.util.UUID

/**
 * The in-app camera, over the recorder. The recording keeps running in its
 * service: CameraX never touches the microphone.
 *
 * A photo is pinned to the moment the shutter was pressed, read from the
 * recording's clock before the camera takes its time to capture. [onCaptured]
 * gets the saved file and that moment; [photoFile] names the file to write.
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun CameraSheet(
    photoFile: suspend () -> File,
    onCaptured: (file: File, timestampMs: Long) -> Unit,
    onError: (String) -> Unit,
    onDismiss: () -> Unit,
) {
    val context = LocalContext.current
    val lifecycleOwner = LocalLifecycleOwner.current
    val sheetState = rememberModalBottomSheetState(skipPartiallyExpanded = true)
    var permitted by remember {
        mutableStateOf(context.checkSelfPermission(Manifest.permission.CAMERA) == PackageManager.PERMISSION_GRANTED)
    }
    val cameraDenied = stringResource(R.string.camera_denied)
    val unavailable = stringResource(R.string.camera_unavailable)
    val permission = rememberLauncherForActivityResult(ActivityResultContracts.RequestPermission()) { granted ->
        permitted = granted
        if (!granted) {
            onError(cameraDenied)
            onDismiss()
        }
    }
    LaunchedEffect(Unit) { if (!permitted) permission.launch(Manifest.permission.CAMERA) }

    var useFront by remember { mutableStateOf(false) }
    var busy by remember { mutableStateOf(false) }
    var pendingShot by remember { mutableStateOf<Long?>(null) }
    val previewView = remember { PreviewView(context).apply { scaleType = PreviewView.ScaleType.FIT_CENTER } }
    val imageCapture = remember {
        ImageCapture.Builder().setCaptureMode(ImageCapture.CAPTURE_MODE_MINIMIZE_LATENCY).build()
    }

    if (permitted) {
        DisposableEffect(useFront) {
            val future = ProcessCameraProvider.getInstance(context)
            var provider: ProcessCameraProvider? = null
            future.addListener({
                try {
                    val p = future.get()
                    provider = p
                    val preview = Preview.Builder().build().also { it.surfaceProvider = previewView.surfaceProvider }
                    val selector = if (useFront) CameraSelector.DEFAULT_FRONT_CAMERA else CameraSelector.DEFAULT_BACK_CAMERA
                    p.unbindAll()
                    p.bindToLifecycle(lifecycleOwner, selector, preview, imageCapture)
                } catch (e: Exception) {
                    onError(unavailable)
                    onDismiss()
                }
            }, ContextCompat.getMainExecutor(context))
            onDispose { provider?.unbindAll() }
        }
    }

    // The file is made off the main thread, then the capture starts.
    pendingShot?.let { timestampMs ->
        LaunchedEffect(timestampMs) {
            val file = runCatching { photoFile() }.getOrElse {
                onError(it.message ?: it.javaClass.simpleName)
                busy = false
                pendingShot = null
                return@LaunchedEffect
            }
            imageCapture.takePicture(
                ImageCapture.OutputFileOptions.Builder(file).build(),
                ContextCompat.getMainExecutor(context),
                object : ImageCapture.OnImageSavedCallback {
                    override fun onImageSaved(output: ImageCapture.OutputFileResults) {
                        busy = false
                        onCaptured(file, timestampMs)
                    }

                    override fun onError(exception: ImageCaptureException) {
                        busy = false
                        file.delete()
                        onError(exception.message ?: unavailable)
                    }
                },
            )
            pendingShot = null
        }
    }

    ModalBottomSheet(onDismissRequest = onDismiss, sheetState = sheetState) {
        Column(
            modifier = Modifier.fillMaxWidth().padding(horizontal = 16.dp, vertical = 8.dp),
            horizontalAlignment = Alignment.CenterHorizontally,
            verticalArrangement = Arrangement.spacedBy(16.dp),
        ) {
            Box(
                Modifier.fillMaxWidth().aspectRatio(3f / 4f).clip(RoundedCornerShape(16.dp)),
                contentAlignment = Alignment.Center,
            ) {
                if (permitted) {
                    AndroidView(factory = { previewView }, modifier = Modifier.fillMaxWidth().aspectRatio(3f / 4f))
                } else {
                    Text(cameraDenied, style = MaterialTheme.typography.bodyMedium)
                }
            }
            Row(
                modifier = Modifier.fillMaxWidth().padding(bottom = 16.dp),
                horizontalArrangement = Arrangement.SpaceEvenly,
                verticalAlignment = Alignment.CenterVertically,
            ) {
                FilledTonalIconButton(onClick = { useFront = !useFront }, enabled = permitted && !busy) {
                    Icon(painterResource(R.drawable.ic_flip_camera), stringResource(R.string.flip_camera))
                }
                FilledIconButton(
                    onClick = {
                        busy = true
                        pendingShot = NativeAudio.snapshot().positionMs
                    },
                    enabled = permitted && !busy,
                    modifier = Modifier.size(72.dp),
                ) {
                    Icon(painterResource(R.drawable.ic_camera), stringResource(R.string.shutter), Modifier.size(32.dp))
                }
                FilledTonalIconButton(onClick = onDismiss) {
                    Icon(painterResource(R.drawable.ic_close), stringResource(R.string.cancel))
                }
            }
        }
    }
}

/** A fresh file name for a photo. */
fun newPhotoName(): String = "${UUID.randomUUID()}.jpg"
