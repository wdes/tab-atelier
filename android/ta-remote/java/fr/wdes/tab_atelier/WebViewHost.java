// Embedded WebView for the tab-atelier share-viewer. Replaces the old
// Intent.ACTION_VIEW launch — the share-viewer now lives INSIDE the
// app instead of bouncing to the system browser.
//
// Why this is a Java helper and not pure Rust+JNI:
//   * subclassing `WebViewClient` to override `onReceivedSslError`
//     (the SSL bypass for the headless's self-signed cert) requires
//     a concrete Java class, not a JNI Proxy — WebViewClient is a
//     class with stub default impls, not an interface.
//   * `WebView` MUST be touched only on the activity's UI thread.
//     The `runOnUiThread` hop guarantees that regardless of which
//     thread Rust calls in from.
//
// All state lives in static fields on this class. We never serve
// more than one WebView at a time (the user picked ONE tab to
// open), so a single static slot is enough.

package fr.wdes.tab_atelier;

import android.app.Activity;
import android.net.http.SslError;
import android.view.View;
import android.view.ViewGroup;
import android.webkit.SslErrorHandler;
import android.webkit.WebSettings;
import android.webkit.WebView;
import android.webkit.WebViewClient;
import android.widget.FrameLayout;

public final class WebViewHost {
    private static volatile WebView current;

    /** Mount a fullscreen WebView pointed at `url`. Idempotent —
     *  replaces any currently-mounted view. */
    public static void show(final Activity activity, final String url) {
        if (activity == null || url == null) return;
        activity.runOnUiThread(new Runnable() {
            @Override public void run() {
                dismissLocked();
                WebView wv = new WebView(activity);
                WebSettings s = wv.getSettings();
                s.setJavaScriptEnabled(true);
                s.setDomStorageEnabled(true);
                // The headless's TLS endpoint serves a self-signed cert
                // (or a CF Origin cert that Android doesn't trust by
                // default). The user explicitly asked for the TLS URL
                // to be reachable from the WebView without manual cert
                // pinning, so we accept any cert here. The bearer
                // token in the URL is the actual authn material — TLS
                // here is a confidentiality channel, not an authn one.
                wv.setWebViewClient(new WebViewClient() {
                    @Override
                    public void onReceivedSslError(WebView view,
                                                   SslErrorHandler handler,
                                                   SslError error) {
                        handler.proceed();
                    }
                });
                FrameLayout.LayoutParams lp = new FrameLayout.LayoutParams(
                        ViewGroup.LayoutParams.MATCH_PARENT,
                        ViewGroup.LayoutParams.MATCH_PARENT);
                activity.addContentView(wv, lp);
                wv.loadUrl(url);
                current = wv;
            }
        });
    }

    /** Remove + destroy the currently-mounted WebView. Returns true
     *  if there was one to dismiss (caller uses this from the Back
     *  key handler to know whether to consume the press). */
    public static boolean dismiss(final Activity activity) {
        if (current == null || activity == null) return false;
        activity.runOnUiThread(new Runnable() {
            @Override public void run() { dismissLocked(); }
        });
        return true;
    }

    /** Internal. Must run on the UI thread. */
    private static void dismissLocked() {
        WebView wv = current;
        if (wv == null) return;
        current = null;
        ViewGroup parent = (ViewGroup) wv.getParent();
        if (parent != null) parent.removeView(wv);
        wv.destroy();
    }
}
