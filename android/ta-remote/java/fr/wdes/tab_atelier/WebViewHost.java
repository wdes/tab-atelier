// Embedded WebView for the tab-atelier share-viewer. Replaces the
// old Intent.ACTION_VIEW launch — the share-viewer now lives INSIDE
// the app, hosted in a fullscreen Dialog so it composites ABOVE the
// NativeActivity window surface that Slint owns.
//
// Why a Dialog and not addContentView:
//   NativeActivity hands its entire window surface to Slint's GL
//   renderer. The View hierarchy inside R.id.content does still
//   exist but never gets composited on top — the native surface IS
//   the visible output. Adding a WebView via addContentView yields a
//   View that's technically attached and laid out (`uiautomator dump`
//   shows it) but never drawn on screen.
//
//   A Dialog opens a SECOND WindowManager.LayoutParams window that
//   the system compositor stacks above the activity window, so the
//   WebView can sit there and we don't fight Slint's render path.

package fr.wdes.tab_atelier;

import android.app.Activity;
import android.app.Dialog;
import android.graphics.Color;
import android.graphics.drawable.ColorDrawable;
import android.net.http.SslError;
import android.view.View;
import android.view.ViewGroup;
import android.view.Window;
import android.view.WindowManager;
import android.webkit.SslErrorHandler;
import android.webkit.WebSettings;
import android.webkit.WebView;
import android.webkit.WebViewClient;

public final class WebViewHost {
    private static volatile Dialog currentDialog;
    private static volatile WebView currentWebView;

    /** Mount a fullscreen WebView pointed at `url` in a Dialog above
     *  the activity. Idempotent — replaces any currently-mounted
     *  Dialog. */
    public static void show(final Activity activity, final String url) {
        if (activity == null || url == null) return;
        activity.runOnUiThread(new Runnable() {
            @Override public void run() {
                dismissLocked();
                // No-title-bar but NOT fullscreen: keep the system
                // status-bar / notification area visible above the
                // dialog. Theme_Black_NoTitleBar_Fullscreen took the
                // notification area too and the WebView overran into
                // it, leaving the wifi / battery icons sitting on top
                // of page content.
                Dialog dialog = new Dialog(activity, android.R.style.Theme_Black_NoTitleBar);
                Window w = dialog.getWindow();
                if (w != null) {
                    w.setBackgroundDrawable(new ColorDrawable(Color.BLACK));
                    w.setLayout(
                            WindowManager.LayoutParams.MATCH_PARENT,
                            WindowManager.LayoutParams.MATCH_PARENT);
                }
                WebView wv = new WebView(activity);
                WebSettings s = wv.getSettings();
                s.setJavaScriptEnabled(true);
                s.setDomStorageEnabled(true);
                // Self-signed origin certs (or CF Origin certs Android
                // doesn't trust by default) — the bearer token in the
                // URL is the authn material; TLS here is just for
                // confidentiality.
                wv.setWebViewClient(new WebViewClient() {
                    @Override
                    public void onReceivedSslError(WebView view,
                                                   SslErrorHandler handler,
                                                   SslError error) {
                        handler.proceed();
                    }
                });
                dialog.setContentView(wv, new ViewGroup.LayoutParams(
                        ViewGroup.LayoutParams.MATCH_PARENT,
                        ViewGroup.LayoutParams.MATCH_PARENT));
                dialog.show();
                wv.loadUrl(url);
                currentDialog = dialog;
                currentWebView = wv;
            }
        });
    }

    /** Dismiss the currently-mounted Dialog (if any). Returns true
     *  if one was up — caller wires this to the Back-key handler so
     *  the press is consumed instead of falling through to the
     *  activity (which would close the app). */
    public static boolean dismiss(final Activity activity) {
        if (currentDialog == null || activity == null) return false;
        activity.runOnUiThread(new Runnable() {
            @Override public void run() { dismissLocked(); }
        });
        return true;
    }

    /** Internal. Must run on the UI thread. */
    private static void dismissLocked() {
        Dialog d = currentDialog;
        WebView wv = currentWebView;
        currentDialog = null;
        currentWebView = null;
        if (wv != null) {
            wv.stopLoading();
            wv.loadUrl("about:blank");
        }
        if (d != null) {
            try { d.dismiss(); } catch (IllegalArgumentException ignored) {}
        }
        if (wv != null) {
            wv.destroy();
        }
    }
}
