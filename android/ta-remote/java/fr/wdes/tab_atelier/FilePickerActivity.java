// Tiny translucent activity that owns the file-picker Activity result on
// behalf of the WebView.
//
// Why this exists: the app runs on android.app.NativeActivity (Slint owns the
// GL surface), and the share-viewer WebView is hosted in a Dialog above it
// (see WebViewHost). A WebView's <input type=file> only works if a
// WebChromeClient.onShowFileChooser launches a picker via
// startActivityForResult and feeds the chosen URIs back — but NativeActivity
// doesn't expose an overridable onActivityResult we can hook from Java. So the
// chrome client starts THIS activity, which runs the standard SAF picker,
// receives the result in its own onActivityResult, and forwards the URIs to
// WebViewHost.deliverFileChooserResult(). Uses the Storage Access Framework
// (ACTION_OPEN_DOCUMENT), so no storage permission is needed.

package fr.wdes.tab_atelier;

import android.app.Activity;
import android.content.Intent;
import android.net.Uri;
import android.os.Bundle;

public final class FilePickerActivity extends Activity {
    private static final int REQ_PICK = 0xF11E;

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        // Re-created after a config change / process death mid-pick: the
        // callback is gone, so just bail rather than launch a second picker.
        if (savedInstanceState != null) {
            finish();
            return;
        }
        Intent intent = new Intent(Intent.ACTION_OPEN_DOCUMENT);
        intent.addCategory(Intent.CATEGORY_OPENABLE);
        intent.setType("*/*");
        intent.putExtra(Intent.EXTRA_ALLOW_MULTIPLE, true);
        try {
            startActivityForResult(intent, REQ_PICK);
        } catch (Exception e) {
            // No picker on the device — release the WebView's pending state.
            WebViewHost.deliverFileChooserResult(null);
            finish();
        }
    }

    @Override
    protected void onActivityResult(int requestCode, int resultCode, Intent data) {
        super.onActivityResult(requestCode, resultCode, data);
        Uri[] uris = null;
        if (requestCode == REQ_PICK && resultCode == RESULT_OK && data != null) {
            if (data.getClipData() != null) {
                int n = data.getClipData().getItemCount();
                uris = new Uri[n];
                for (int i = 0; i < n; i++) {
                    uris[i] = data.getClipData().getItemAt(i).getUri();
                }
            } else if (data.getData() != null) {
                uris = new Uri[] { data.getData() };
            }
        }
        // Always deliver (null on cancel) so the WebView doesn't get stuck
        // ignoring every later file-input tap.
        WebViewHost.deliverFileChooserResult(uris);
        finish();
    }
}
