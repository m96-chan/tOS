package io.github.m96chan.tos;

import android.app.Activity;
import android.content.ClipData;
import android.content.ClipboardManager;
import android.content.Context;
import android.content.res.Configuration;
import android.graphics.Color;
import android.graphics.Insets;
import android.os.Build;
import android.os.Bundle;
import android.text.Editable;
import android.text.InputType;
import android.text.SpannableStringBuilder;
import android.view.GestureDetector;
import android.view.KeyEvent;
import android.view.MotionEvent;
import android.view.ScaleGestureDetector;
import android.view.Surface;
import android.view.SurfaceHolder;
import android.view.SurfaceView;
import android.view.View;
import android.view.WindowInsets;
import android.view.WindowManager;
import android.view.inputmethod.BaseInputConnection;
import android.view.inputmethod.EditorInfo;
import android.view.inputmethod.InputConnection;
import android.view.inputmethod.InputMethodManager;
import android.widget.Button;
import android.widget.HorizontalScrollView;
import android.widget.LinearLayout;
import android.widget.PopupMenu;
import android.widget.TextView;
import android.widget.Toast;
import java.io.File;
import java.nio.charset.StandardCharsets;
import java.util.Locale;
import java.util.concurrent.ScheduledThreadPoolExecutor;
import java.util.concurrent.TimeUnit;
import java.util.function.LongConsumer;

/** A native Android window around the existing Rust compositor. */
public final class MainActivity extends Activity {
    private static final int BG = Color.rgb(16, 16, 20);
    private static final int BAR = Color.rgb(25, 23, 30);
    private static final int FG = Color.rgb(221, 214, 225);
    private static final int GREEN = Color.rgb(145, 180, 135);
    private final ScheduledThreadPoolExecutor worker = new ScheduledThreadPoolExecutor(1);
    // Only the worker owns the native handle and calls JNI.
    private long engine;
    private long lastGrid;
    private volatile boolean closing;
    private volatile boolean visible;
    private TerminalView terminal;
    private TextView title, sizeLabel, composition;
    private Button ctrlButton, altButton;
    private boolean ctrl, alt, keyboardVisible;
    private float fontSp;

    @Override public void onCreate(Bundle state) {
        super.onCreate(state);
        getWindow().setSoftInputMode(WindowManager.LayoutParams.SOFT_INPUT_ADJUST_RESIZE);
        worker.setExecuteExistingDelayedTasksAfterShutdownPolicy(false);
        fontSp = getPreferences(MODE_PRIVATE).getFloat("fontSp", 10.5f);
        if (!Float.isFinite(fontSp)) fontSp = 10.5f;
        fontSp = Math.max(8f, Math.min(24f, fontSp));

        LinearLayout root = new LinearLayout(this);
        root.setOrientation(LinearLayout.VERTICAL);
        root.setBackgroundColor(BG);
        LinearLayout top = row();
        title = label("tOS", 16);
        title.setTextColor(GREEN);
        title.setPadding(dp(12), 0, 0, 0);
        top.addView(title, new LinearLayout.LayoutParams(0, dp(48), 1));
        top.addView(button("A−", "Smaller text", () -> changeFont(fontSp - .5f)));
        sizeLabel = label("", 12);
        top.addView(sizeLabel, new LinearLayout.LayoutParams(dp(40), dp(48)));
        top.addView(button("A+", "Larger text", () -> changeFont(fontSp + .5f)));
        top.addView(button("⌨", "Show or hide keyboard", this::toggleKeyboard));
        Button menu = button("⋮", "Session menu", () -> {});
        menu.setOnClickListener(v -> showMenu(menu));
        top.addView(menu);
        root.addView(top);

        terminal = new TerminalView();
        root.addView(terminal, new LinearLayout.LayoutParams(-1, 0, 1));
        composition = label("", 14);
        composition.setPadding(dp(10), 0, dp(10), 0);
        composition.setTextColor(GREEN);
        composition.setBackgroundColor(BAR);
        root.addView(composition, new LinearLayout.LayoutParams(-1, dp(22)));

        HorizontalScrollView scroll = new HorizontalScrollView(this);
        scroll.setHorizontalScrollBarEnabled(false);
        LinearLayout keys = row();
        keys.addView(button("Esc", "Escape", () -> special(KeyEvent.KEYCODE_ESCAPE)));
        ctrlButton = button("Ctrl", "Control modifier", () -> { ctrl = !ctrl; modifiersChanged(); });
        keys.addView(ctrlButton);
        altButton = button("Alt", "Alt modifier", () -> { alt = !alt; modifiersChanged(); });
        keys.addView(altButton);
        keys.addView(button("Tab", "Tab", () -> special(KeyEvent.KEYCODE_TAB)));
        keys.addView(button("←", "Left", () -> special(KeyEvent.KEYCODE_DPAD_LEFT)));
        keys.addView(button("↓", "Down", () -> special(KeyEvent.KEYCODE_DPAD_DOWN)));
        keys.addView(button("↑", "Up", () -> special(KeyEvent.KEYCODE_DPAD_UP)));
        keys.addView(button("→", "Right", () -> special(KeyEvent.KEYCODE_DPAD_RIGHT)));
        scroll.addView(keys);
        root.addView(scroll, new LinearLayout.LayoutParams(-1, dp(48)));
        if (Build.VERSION.SDK_INT >= 30) {
            getWindow().setDecorFitsSystemWindows(false);
            root.setOnApplyWindowInsetsListener((view, insets) -> {
                Insets system = insets.getInsets(WindowInsets.Type.systemBars());
                Insets ime = insets.getInsets(WindowInsets.Type.ime());
                keyboardVisible = insets.isVisible(WindowInsets.Type.ime());
                view.setPadding(system.left, system.top, system.right, Math.max(system.bottom, ime.bottom));
                return insets;
            });
        } else { root.setFitsSystemWindows(true); }
        setContentView(root);
        updateSizeLabel();
        terminal.requestFocus();

        String vmIssue = VmService.unavailable(this);
        if (vmIssue != null) {
            new android.app.AlertDialog.Builder(this).setTitle("Debian setup").setMessage(vmIssue)
                .setPositiveButton("Close", (dialog, which) -> finish()).show();
            return;
        }

        File home = new File(getFilesDir(), "home");
        new File(home, "tmp").mkdirs();
        final float pixels = fontPixels();
        worker.execute(() -> {
            try {
                message("Preparing tools…");
                Userland.prepare(this);
                VmService.start(this);
                engine = NativeSession.create(home.getAbsolutePath(), pixels);
                if (engine == 0) { message("Could not start the Debian session"); return; }
                runOnUiThread(() -> composition.setText(""));
                runOnUiThread(() -> terminal.bindSurface());
                step();
            } catch (Throwable error) {
                android.util.Log.e("tOS", "Could not start session", error);
                message("Could not start tOS: " + error.getMessage());
            }
        });
    }

    private void step() {
        if (closing || engine == 0) return;
        if (visible) {
            int result = NativeSession.step(engine);
            long grid = NativeSession.grid(engine);
            if (grid != lastGrid) {
                lastGrid = grid;
                runOnUiThread(() -> title.setText("tOS  " + (grid >>> 32) + "×" + (grid & 0xffffffffL)));
            }
            if (result < 0) {
                message(VmService.lastFailure != null ? "Debian: " + VmService.lastFailure : "Session ended — reopen tOS to start again");
                return;
            }
        }
        if (!closing) worker.schedule(this::step, visible ? 33 : 250, TimeUnit.MILLISECONDS);
    }

    private void session(LongConsumer action) {
        if (closing) return;
        worker.execute(() -> { if (engine != 0) action.accept(engine); });
    }

    private int dp(float value) { return Math.round(value * getResources().getDisplayMetrics().density); }
    private float fontPixels() { return fontSp * getResources().getDisplayMetrics().scaledDensity; }
    private LinearLayout row() {
        LinearLayout row = new LinearLayout(this);
        row.setOrientation(LinearLayout.HORIZONTAL);
        row.setBackgroundColor(BAR);
        return row;
    }
    private TextView label(String text, float size) {
        TextView view = new TextView(this);
        view.setText(text); view.setTextSize(size); view.setTextColor(FG);
        view.setGravity(android.view.Gravity.CENTER_VERTICAL);
        return view;
    }
    private Button button(String text, String description, Runnable action) {
        Button b = new Button(this);
        b.setText(text); b.setTextSize(13); b.setTextColor(FG);
        b.setAllCaps(false); b.setMinWidth(0); b.setMinimumWidth(0);
        b.setPadding(0, 0, 0, 0); b.setContentDescription(description);
        b.setLayoutParams(new LinearLayout.LayoutParams(dp(48), dp(48)));
        b.setOnClickListener(v -> { action.run(); terminal.requestFocus(); });
        return b;
    }
    private void message(String text) {
        runOnUiThread(() -> { if (!closing) { composition.setText(text); Toast.makeText(this, text, Toast.LENGTH_LONG).show(); } });
    }
    private void changeFont(float size) {
        fontSp = Math.max(8f, Math.min(24f, Math.round(size * 2) / 2f));
        getPreferences(MODE_PRIVATE).edit().putFloat("fontSp", fontSp).apply();
        updateSizeLabel(); terminal.resize();
    }
    private void updateSizeLabel() { sizeLabel.setText(String.format(Locale.ROOT, "%.1f", fontSp)); }
    private void modifiersChanged() {
        ctrlButton.setTextColor(ctrl ? GREEN : FG); ctrlButton.setSelected(ctrl);
        altButton.setTextColor(alt ? GREEN : FG); altButton.setSelected(alt);
    }
    private int takeModifiers() {
        int mods = (ctrl ? 4 : 0) | (alt ? 2 : 0);
        ctrl = alt = false; modifiersChanged();
        return mods;
    }
    private void special(int code) {
        int mods = takeModifiers();
        session(h -> NativeSession.key(h, code, 0, mods, false));
    }
    private void commit(String text) {
        if (text.isEmpty()) return;
        int mods = takeModifiers();
        session(h -> NativeSession.text(h, text, mods, false));
    }
    private void toggleKeyboard() {
        terminal.requestFocus();
        InputMethodManager ime = (InputMethodManager)getSystemService(INPUT_METHOD_SERVICE);
        if (keyboardVisible) ime.hideSoftInputFromWindow(terminal.getWindowToken(), 0);
        else ime.showSoftInput(terminal, InputMethodManager.SHOW_IMPLICIT);
        if (Build.VERSION.SDK_INT < 30) keyboardVisible = !keyboardVisible;
    }
    private void showMenu(View anchor) {
        PopupMenu menu = new PopupMenu(this, anchor);
        String[] labels = {"Split left / right", "Split top / bottom", "Close pane", "Zoom pane", "Select text", "Copy selection", "New workspace", "Next workspace"};
        for (int i = 0; i < labels.length; i++) menu.getMenu().add(0, i, i, labels[i]);
        menu.getMenu().add(0, 8, 8, "Paste");
        menu.getMenu().add(0, 9, 9, "Shut down Debian");
        menu.setOnMenuItemClickListener(item -> {
            int action = item.getItemId();
            if (action == 9) {
                VmService.shutdown(this);
            } else if (action == 8) {
                ClipboardManager clipboard = (ClipboardManager)getSystemService(CLIPBOARD_SERVICE);
                ClipData data = clipboard.getPrimaryClip();
                if (data != null && data.getItemCount() > 0) {
                    String text = data.getItemAt(0).coerceToText(this).toString();
                    session(h -> NativeSession.text(h, text, 0, true));
                }
            } else {
                session(h -> {
                    NativeSession.action(h, action);
                    if (action == 5) {
                        byte[] bytes = NativeSession.clipboard(h);
                        if (bytes != null && bytes.length > 0) {
                            String text = new String(bytes, StandardCharsets.UTF_8);
                            runOnUiThread(() -> ((ClipboardManager)getSystemService(CLIPBOARD_SERVICE)).setPrimaryClip(ClipData.newPlainText("tOS", text)));
                        }
                    }
                });
            }
            terminal.requestFocus(); return true;
        });
        menu.show();
    }

    @Override protected void onStart() { super.onStart(); visible = true; }
    @Override protected void onStop() { visible = false; super.onStop(); }
    @Override public void onConfigurationChanged(Configuration config) {
        super.onConfigurationChanged(config);
        terminal.resize();
    }
    @Override protected void onDestroy() {
        closing = true;
        worker.execute(() -> { if (engine != 0) { NativeSession.destroy(engine); engine = 0; } });
        worker.shutdown();
        super.onDestroy();
    }

    private final class TerminalView extends SurfaceView implements SurfaceHolder.Callback {
        private final GestureDetector gestures;
        private final ScaleGestureDetector scale;
        private float pinchFont, pinchScale;
        private float scrollPixels;

        TerminalView() {
            super(MainActivity.this);
            setFocusable(true); setFocusableInTouchMode(true);
            setContentDescription("tOS terminal");
            getHolder().addCallback(this);
            scale = new ScaleGestureDetector(MainActivity.this, new ScaleGestureDetector.SimpleOnScaleGestureListener() {
                @Override public boolean onScaleBegin(ScaleGestureDetector d) { pinchFont = fontSp; pinchScale = 1; return true; }
                @Override public boolean onScale(ScaleGestureDetector d) { pinchScale *= d.getScaleFactor(); return true; }
                @Override public void onScaleEnd(ScaleGestureDetector d) { changeFont(pinchFont * pinchScale); }
            });
            gestures = new GestureDetector(MainActivity.this, new GestureDetector.SimpleOnGestureListener() {
                @Override public boolean onDown(MotionEvent e) { scrollPixels = 0; return true; }
                @Override public boolean onSingleTapUp(MotionEvent e) {
                    requestFocus(); float x = e.getX(), y = e.getY();
                    session(h -> NativeSession.pointer(h, x, y, 0)); return true;
                }
                @Override public void onLongPress(MotionEvent e) { showMenu(terminal); }
                @Override public boolean onScroll(MotionEvent first, MotionEvent last, float dx, float dy) {
                    scrollPixels += dy;
                    float threshold = Math.max(8, fontPixels());
                    int steps = Math.min(8, (int)(Math.abs(scrollPixels) / threshold));
                    if (steps > 0) {
                        int wheel = scrollPixels > 0 ? -1 : 1;
                        scrollPixels %= threshold;
                        float x = last.getX(), y = last.getY();
                        session(h -> { for (int i = 0; i < steps; i++) NativeSession.pointer(h, x, y, wheel); });
                    }
                    return true;
                }
            });
        }
        void bindSurface() {
            if (closing) return;
            Surface surface = getHolder().getSurface();
            session(h -> NativeSession.surface(h, surface.isValid() ? surface : null));
            resize();
        }
        void resize() {
            int width = getWidth(), height = getHeight();
            float font = fontPixels();
            if (width > 0 && height > 0) session(h -> NativeSession.resize(h, width, height, font));
        }
        @Override public void surfaceCreated(SurfaceHolder holder) { bindSurface(); }
        @Override public void surfaceChanged(SurfaceHolder holder, int format, int width, int height) { bindSurface(); }
        @Override public void surfaceDestroyed(SurfaceHolder holder) {
            if (closing) return;
            try { worker.submit(() -> { if (engine != 0) NativeSession.surface(engine, null); }).get(1, TimeUnit.SECONDS); }
            catch (Exception ignored) { /* A queued detach still owns the native reference until it runs. */ }
        }
        @Override public boolean onTouchEvent(MotionEvent event) {
            scale.onTouchEvent(event);
            if (!scale.isInProgress() && event.getPointerCount() == 1) gestures.onTouchEvent(event);
            return true;
        }
        @Override public boolean onCheckIsTextEditor() { return true; }
        @Override public InputConnection onCreateInputConnection(EditorInfo info) {
            info.inputType = InputType.TYPE_CLASS_TEXT | InputType.TYPE_TEXT_FLAG_MULTI_LINE | InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS;
            info.imeOptions = EditorInfo.IME_FLAG_NO_EXTRACT_UI | EditorInfo.IME_FLAG_NO_FULLSCREEN | EditorInfo.IME_ACTION_NONE;
            info.initialSelStart = info.initialSelEnd = 0;
            return new BaseInputConnection(this, true) {
                private final Editable editable = new SpannableStringBuilder();
                @Override public Editable getEditable() { return editable; }
                private void clear() { editable.clear(); removeComposingSpans(editable); composition.setText(""); }
                @Override public boolean setComposingText(CharSequence text, int cursor) {
                    super.setComposingText(text, cursor); composition.setText(editable); return true;
                }
                @Override public boolean commitText(CharSequence text, int cursor) { commit(text.toString()); clear(); return true; }
                @Override public boolean finishComposingText() {
                    if (editable.length() > 0) commit(editable.toString());
                    clear(); return true;
                }
                @Override public boolean deleteSurroundingText(int before, int after) {
                    if (editable.length() > 0) { super.deleteSurroundingText(before, after); composition.setText(editable); }
                    else {
                        for (int i = 0; i < Math.min(before, 1024); i++) special(KeyEvent.KEYCODE_DEL);
                        for (int i = 0; i < Math.min(after, 1024); i++) special(KeyEvent.KEYCODE_FORWARD_DEL);
                    }
                    return true;
                }
                @Override public boolean deleteSurroundingTextInCodePoints(int before, int after) {
                    if (editable.length() > 0) {
                        super.deleteSurroundingTextInCodePoints(before, after); composition.setText(editable); return true;
                    }
                    return deleteSurroundingText(before, after);
                }
                @Override public boolean sendKeyEvent(KeyEvent e) { return handleKey(e); }
                @Override public boolean performEditorAction(int action) { special(KeyEvent.KEYCODE_ENTER); return true; }
            };
        }
        private boolean handleKey(KeyEvent event) {
            int code = event.getKeyCode();
            if (code == KeyEvent.KEYCODE_BACK || code == KeyEvent.KEYCODE_VOLUME_UP || code == KeyEvent.KEYCODE_VOLUME_DOWN) return false;
            if (KeyEvent.isModifierKey(code)) return true;
            boolean release = event.getAction() == KeyEvent.ACTION_UP;
            int mods = (event.isShiftPressed() ? 1 : 0) | (event.isAltPressed() ? 2 : 0) | (event.isCtrlPressed() ? 4 : 0) | (event.isMetaPressed() ? 8 : 0);
            if (!release) mods |= takeModifiers();
            int unicode = event.getUnicodeChar(event.getMetaState() & ~(KeyEvent.META_CTRL_MASK | KeyEvent.META_ALT_MASK));
            final int modifiers = mods;
            session(h -> NativeSession.key(h, code, unicode, modifiers, release));
            return true;
        }
        @Override public boolean onKeyDown(int code, KeyEvent event) { return handleKey(event) || super.onKeyDown(code, event); }
        @Override public boolean onKeyUp(int code, KeyEvent event) { return handleKey(event) || super.onKeyUp(code, event); }
    }
}
