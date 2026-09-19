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
import android.view.HapticFeedbackConstants;
import android.view.InputDevice;
import android.view.KeyEvent;
import android.view.MenuItem;
import android.view.MotionEvent;
import android.view.ScaleGestureDetector;
import android.view.Surface;
import android.view.SurfaceHolder;
import android.view.SurfaceView;
import android.view.View;
import android.view.ViewConfiguration;
import android.view.WindowInsets;
import android.view.WindowManager;
import android.view.inputmethod.BaseInputConnection;
import android.view.inputmethod.EditorInfo;
import android.view.inputmethod.InputConnection;
import android.view.inputmethod.InputMethodManager;
import android.view.inputmethod.InputMethodSubtype;
import android.widget.Button;
import android.widget.FrameLayout;
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
    // Menu ids: 0..7 are the native actions, and the rest are this side's own.
    private static final int COPY = 5, PASTE = 8, SHUTDOWN = 9, MORE = 10;
    private final ScheduledThreadPoolExecutor worker = new ScheduledThreadPoolExecutor(1);
    // Only the worker owns the native handle and calls JNI.
    private long engine;
    private long lastGrid;
    private volatile boolean closing;
    private volatile boolean visible;
    private TerminalView terminal;
    private LinearLayout root, toolbarControls, keys;
    private HorizontalScrollView toolbarScroll, keysScroll;
    // A one pixel view moved under the finger, so a menu opens where it was
    // asked for rather than in a corner of the screen.
    private View touchAnchor;
    private TextView title, sizeLabel, composition;
    private Button ctrlButton, altButton, menuButton;
    private boolean ctrl, alt, keyboardVisible;
    private float fontSp;

    @Override public void onCreate(Bundle state) {
        super.onCreate(state);
        getWindow().setSoftInputMode(WindowManager.LayoutParams.SOFT_INPUT_ADJUST_RESIZE);
        worker.setExecuteExistingDelayedTasksAfterShutdownPolicy(false);
        fontSp = getPreferences(MODE_PRIVATE).getFloat("fontSp", 10.5f);
        if (!Float.isFinite(fontSp)) fontSp = 10.5f;
        fontSp = Math.max(8f, Math.min(24f, fontSp));

        root = new LinearLayout(this);
        root.setOrientation(LinearLayout.VERTICAL);
        root.setBackgroundColor(BG);
        LinearLayout top = row();
        toolbarControls = row();
        title = label("tOS", 16);
        title.setTextColor(GREEN);
        title.setPadding(dp(12), 0, 0, 0);
        toolbarControls.addView(title, new LinearLayout.LayoutParams(dp(110), dp(48)));
        toolbarControls.addView(button("A−", "Smaller text", () -> changeFont(fontSp - .5f)));
        sizeLabel = label("", 12);
        toolbarControls.addView(sizeLabel, new LinearLayout.LayoutParams(dp(40), dp(48)));
        toolbarControls.addView(button("A+", "Larger text", () -> changeFont(fontSp + .5f)));
        toolbarControls.addView(button("⌨", "Show or hide keyboard", this::toggleKeyboard));
        toolbarScroll = new HorizontalScrollView(this);
        toolbarScroll.setHorizontalScrollBarEnabled(false);
        toolbarScroll.addView(toolbarControls);
        top.addView(toolbarScroll, new LinearLayout.LayoutParams(0, dp(48), 1));
        menuButton = button("⋮", "Session menu", () -> {});
        menuButton.setOnClickListener(v -> showMenu(menuButton));
        top.addView(menuButton);
        root.addView(top);

        FrameLayout stage = new FrameLayout(this);
        terminal = new TerminalView();
        stage.addView(terminal, new FrameLayout.LayoutParams(-1, -1));
        touchAnchor = new View(this);
        stage.addView(touchAnchor, new FrameLayout.LayoutParams(1, 1));
        root.addView(stage, new LinearLayout.LayoutParams(-1, 0, 1));
        composition = label("", 14);
        composition.setPadding(dp(10), 0, dp(10), 0);
        composition.setTextColor(GREEN);
        composition.setBackgroundColor(BAR);
        root.addView(composition, new LinearLayout.LayoutParams(-1, dp(22)));

        keysScroll = new HorizontalScrollView(this);
        keysScroll.setHorizontalScrollBarEnabled(false);
        keys = row();
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
        keysScroll.addView(keys);
        root.addView(keysScroll, new LinearLayout.LayoutParams(-1, dp(48)));
        if (Build.VERSION.SDK_INT >= 30) {
            getWindow().setDecorFitsSystemWindows(false);
            root.setOnApplyWindowInsetsListener((view, insets) -> {
                Insets system = insets.getInsets(WindowInsets.Type.systemBars() | WindowInsets.Type.captionBar());
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
                runOnUiThread(() -> {
                    boolean focused = hasWindowFocus();
                    session(h -> NativeSession.focus(h, focused));
                });
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
    /** A backspace of this side's own, which no armed modifier belongs to. */
    private void backspace() {
        session(h -> NativeSession.key(h, KeyEvent.KEYCODE_DEL, 0, 0, false));
    }
    /**
     * Whether what the keyboard composes should go straight into the pane.
     *
     * A shell answers a character at a time — completion, history search and
     * ^C all happen long before a word is finished — so ASCII has no reason
     * to wait in the composition bar for a commit a Latin keyboard was only
     * going to send at the space bar. A keyboard composing Japanese, Chinese
     * or Korean is the one case where the wait is the point: its romaji are
     * on their way to becoming something else, and typing them through would
     * put a `k` in the pane that has to be taken back a keystroke later.
     */
    private boolean directKeyboard() {
        InputMethodManager ime = (InputMethodManager)getSystemService(INPUT_METHOD_SERVICE);
        InputMethodSubtype subtype = ime == null ? null : ime.getCurrentInputMethodSubtype();
        if (subtype == null) return true;
        String language = subtype.getLanguageTag();
        if (language == null || language.isEmpty()) language = subtype.getLocale();
        if (language == null) return true;
        language = language.toLowerCase(Locale.ROOT);
        return !(language.startsWith("ja") || language.startsWith("zh") || language.startsWith("ko"));
    }
    private static boolean ascii(CharSequence text) {
        for (int i = 0; i < text.length(); i++) {
            char c = text.charAt(i);
            if (c < ' ' || c > '~') return false;
        }
        return text.length() > 0;
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
        menu.getMenu().add(0, PASTE, PASTE, "Paste");
        menu.getMenu().add(0, SHUTDOWN, SHUTDOWN, "Shut down Debian");
        menu.setOnMenuItemClickListener(this::onMenuItem);
        menu.show();
    }
    /**
     * What a long press opens: the clipboard, at the finger.
     *
     * Panes and workspaces are a thing somebody arranges once and leaves
     * alone, and they have the top bar's ⋮ for it. Copy and paste are the
     * opposite — a phone has no ctrl+shift+v, and a PATH typed by hand on a
     * soft keyboard is a typo waiting to happen — so the gesture that is
     * already under the text goes to the clipboard, and the rest is behind
     * "More…".
     */
    private void showClipboardMenu(float x, float y) {
        touchAnchor.setX(Math.max(0, Math.min(terminal.getWidth() - 1, x)));
        touchAnchor.setY(Math.max(0, Math.min(terminal.getHeight() - 1, y)));
        PopupMenu menu = new PopupMenu(this, touchAnchor);
        String pending = clipboardText();
        menu.getMenu().add(0, PASTE, 0, pending == null ? "Paste (clipboard empty)" : "Paste").setEnabled(pending != null);
        menu.getMenu().add(0, COPY, 1, "Copy selection");
        menu.getMenu().add(0, MORE, 2, "More…");
        menu.setOnMenuItemClickListener(this::onMenuItem);
        menu.show();
    }
    private boolean onMenuItem(MenuItem item) {
        int action = item.getItemId();
        if (action == MORE) { showMenu(touchAnchor); return true; }
        if (action == SHUTDOWN) VmService.shutdown(this);
        else if (action == PASTE) paste();
        else if (action == COPY) copySelection();
        else session(h -> NativeSession.action(h, action));
        terminal.requestFocus();
        return true;
    }
    /** Yank whatever the pane has selected, and hand it to Android. */
    private void copySelection() {
        session(h -> {
            NativeSession.action(h, COPY);
            byte[] bytes = NativeSession.clipboard(h);
            if (bytes == null || bytes.length == 0) { message("Nothing to copy — long press and drag over text"); return; }
            String text = new String(bytes, StandardCharsets.UTF_8);
            runOnUiThread(() -> ((ClipboardManager)getSystemService(CLIPBOARD_SERVICE)).setPrimaryClip(ClipData.newPlainText("tOS", text)));
        });
    }
    private void paste() {
        String text = clipboardText();
        if (text == null) { message("The Android clipboard is empty"); return; }
        session(h -> NativeSession.text(h, text, 0, true));
    }
    /** The Android clipboard as text, or null when there is nothing in it. */
    private String clipboardText() {
        ClipData data = ((ClipboardManager)getSystemService(CLIPBOARD_SERVICE)).getPrimaryClip();
        if (data == null || data.getItemCount() == 0) return null;
        CharSequence text = data.getItemAt(0).coerceToText(this);
        return text == null || text.length() == 0 ? null : text.toString();
    }

    @Override protected void onStart() { super.onStart(); visible = true; }
    @Override protected void onStop() {
        visible = false;
        session(h -> NativeSession.focus(h, false));
        super.onStop();
    }
    @Override public void onWindowFocusChanged(boolean gained) {
        super.onWindowFocusChanged(gained);
        session(h -> NativeSession.focus(h, gained));
    }
    @Override public void onConfigurationChanged(Configuration config) {
        super.onConfigurationChanged(config);
        // A window moved to another display can change density without an
        // Activity restart. LayoutParams contain pixels, not dp, so rebuild
        // chrome dimensions before sizing the terminal's new Surface.
        title.setTextSize(16);
        title.setPadding(dp(12), 0, 0, 0);
        title.setLayoutParams(new LinearLayout.LayoutParams(dp(110), dp(48)));
        sizeLabel.setTextSize(12);
        sizeLabel.setLayoutParams(new LinearLayout.LayoutParams(dp(40), dp(48)));
        for (int i = 0; i < toolbarControls.getChildCount(); i++) {
            View child = toolbarControls.getChildAt(i);
            if (child instanceof Button) {
                ((Button)child).setTextSize(13);
                child.setLayoutParams(new LinearLayout.LayoutParams(dp(48), dp(48)));
            }
        }
        for (int i = 0; i < keys.getChildCount(); i++) {
            Button key = (Button)keys.getChildAt(i);
            key.setTextSize(13);
            key.setLayoutParams(new LinearLayout.LayoutParams(dp(48), dp(48)));
        }
        menuButton.setTextSize(13);
        menuButton.setLayoutParams(new LinearLayout.LayoutParams(dp(48), dp(48)));
        toolbarScroll.getLayoutParams().height = dp(48);
        toolbarScroll.requestLayout();
        keysScroll.getLayoutParams().height = dp(48);
        keysScroll.requestLayout();
        composition.setTextSize(14);
        composition.setPadding(dp(10), 0, dp(10), 0);
        composition.getLayoutParams().height = dp(22);
        composition.requestLayout();
        root.requestApplyInsets();
        terminal.post(terminal::resize);
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
        private final int slop;
        private float pinchFont, pinchScale;
        private float scrollPixels;
        // A long press owns the gesture from the moment it fires; the press
        // itself waits for the first real movement, so a hold that never
        // drags leaves no one-cell highlight behind the menu.
        private boolean selecting, dragging;
        private float pressX, pressY;
        private int mouseButton = NativeSession.BUTTON_NONE;
        private boolean hostContextClick;
        private float wheelX, wheelY;

        TerminalView() {
            super(MainActivity.this);
            slop = ViewConfiguration.get(MainActivity.this).getScaledTouchSlop();
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
                    session(h -> {
                        NativeSession.pointer(h, x, y, NativeSession.BUTTON_LEFT, NativeSession.POINTER_PRESS, 0);
                        NativeSession.pointer(h, x, y, NativeSession.BUTTON_LEFT, NativeSession.POINTER_RELEASE, 0);
                    });
                    return true;
                }
                @Override public void onLongPress(MotionEvent e) { beginSelection(e); }
                @Override public boolean onScroll(MotionEvent first, MotionEvent last, float dx, float dy) {
                    scrollPixels += dy;
                    float threshold = Math.max(8, fontPixels());
                    int steps = Math.min(8, (int)(Math.abs(scrollPixels) / threshold));
                    if (steps > 0) {
                        int wheel = scrollPixels > 0 ? -1 : 1;
                        scrollPixels %= threshold;
                        float x = last.getX(), y = last.getY();
                        int button = wheel > 0 ? NativeSession.WHEEL_UP : NativeSession.WHEEL_DOWN;
                        session(h -> { for (int i = 0; i < steps; i++) NativeSession.pointer(h, x, y, button, NativeSession.POINTER_PRESS, 0); });
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
            if (event.isFromSource(InputDevice.SOURCE_MOUSE)) return onMouseTouch(event);
            if (selecting) {
                if (event.getActionMasked() != MotionEvent.ACTION_DOWN) return dragSelection(event);
                // A fresh gesture while a selection is open means its end was
                // lost somewhere; drop it rather than holding the finger.
                endSelection(event.getX(), event.getY(), false);
            }
            scale.onTouchEvent(event);
            if (!scale.isInProgress() && event.getPointerCount() == 1) gestures.onTouchEvent(event);
            return true;
        }
        private int pointerModifiers(MotionEvent event) {
            int state = event.getMetaState();
            return ((state & KeyEvent.META_SHIFT_ON) != 0 ? 1 : 0)
                | ((state & KeyEvent.META_ALT_ON) != 0 ? 2 : 0)
                | ((state & KeyEvent.META_CTRL_ON) != 0 ? 4 : 0)
                | ((state & KeyEvent.META_META_ON) != 0 ? 8 : 0);
        }
        private void mouseEvent(MotionEvent event, int button, int action) {
            float x = event.getX(), y = event.getY();
            int modifiers = pointerModifiers(event);
            session(h -> NativeSession.pointer(h, x, y, button, action, modifiers));
        }
        private int mouseButton(int androidButton) {
            if (androidButton == MotionEvent.BUTTON_PRIMARY) return NativeSession.BUTTON_LEFT;
            if (androidButton == MotionEvent.BUTTON_SECONDARY) return NativeSession.BUTTON_RIGHT;
            if (androidButton == MotionEvent.BUTTON_TERTIARY) return NativeSession.BUTTON_MIDDLE;
            return NativeSession.BUTTON_NONE;
        }
        private boolean onMouseTouch(MotionEvent event) {
            switch (event.getActionMasked()) {
                case MotionEvent.ACTION_DOWN:
                    requestFocus();
                    // Secondary/middle buttons also generate generic button events.
                    // Do not send their following ACTION_DOWN a second time.
                    if (mouseButton == NativeSession.BUTTON_NONE &&
                        (event.getButtonState() & MotionEvent.BUTTON_PRIMARY) != 0) {
                        mouseButton = NativeSession.BUTTON_LEFT;
                        mouseEvent(event, mouseButton, NativeSession.POINTER_PRESS);
                    }
                    return true;
                case MotionEvent.ACTION_MOVE:
                    mouseEvent(event, mouseButton, mouseButton == NativeSession.BUTTON_NONE
                        ? NativeSession.POINTER_MOTION : NativeSession.POINTER_DRAG);
                    return true;
                case MotionEvent.ACTION_UP:
                case MotionEvent.ACTION_CANCEL:
                    if (mouseButton == NativeSession.BUTTON_LEFT ||
                        mouseButton != NativeSession.BUTTON_NONE &&
                        (event.getActionMasked() == MotionEvent.ACTION_CANCEL || event.getButtonState() == 0)) {
                        mouseEvent(event, mouseButton, NativeSession.POINTER_RELEASE);
                        mouseButton = NativeSession.BUTTON_NONE;
                    }
                    return true;
                default:
                    return true;
            }
        }
        private void wheel(MotionEvent event, float amount, boolean horizontal) {
            if (!Float.isFinite(amount)) return;
            if (horizontal) wheelX += amount; else wheelY += amount;
            float pending = horizontal ? wheelX : wheelY;
            int steps = Math.min(8, (int)Math.abs(pending));
            if (steps == 0) return;
            if (horizontal) wheelX -= Math.copySign(steps, pending);
            else wheelY -= Math.copySign(steps, pending);
            int button = horizontal
                ? (pending > 0 ? NativeSession.WHEEL_RIGHT : NativeSession.WHEEL_LEFT)
                : (pending > 0 ? NativeSession.WHEEL_UP : NativeSession.WHEEL_DOWN);
            float x = event.getX(), y = event.getY();
            int modifiers = pointerModifiers(event);
            session(h -> {
                for (int i = 0; i < steps; i++)
                    NativeSession.pointer(h, x, y, button, NativeSession.POINTER_PRESS, modifiers);
            });
        }
        @Override public boolean onGenericMotionEvent(MotionEvent event) {
            if (!event.isFromSource(InputDevice.SOURCE_CLASS_POINTER)) return super.onGenericMotionEvent(event);
            switch (event.getActionMasked()) {
                case MotionEvent.ACTION_SCROLL:
                    wheel(event, event.getAxisValue(MotionEvent.AXIS_VSCROLL), false);
                    wheel(event, event.getAxisValue(MotionEvent.AXIS_HSCROLL), true);
                    return true;
                case MotionEvent.ACTION_HOVER_ENTER:
                case MotionEvent.ACTION_HOVER_MOVE:
                    mouseEvent(event, NativeSession.BUTTON_NONE, NativeSession.POINTER_MOTION);
                    return true;
                case MotionEvent.ACTION_BUTTON_PRESS:
                case MotionEvent.ACTION_BUTTON_RELEASE:
                    int button = mouseButton(event.getActionButton());
                    if (button == NativeSession.BUTTON_NONE || button == NativeSession.BUTTON_LEFT) return true;
                    if (event.getActionMasked() == MotionEvent.ACTION_BUTTON_PRESS) {
                        requestFocus();
                        // Shift+right-click is the host clipboard/menu escape
                        // hatch; an unmodified right-click reaches the guest.
                        if (button == NativeSession.BUTTON_RIGHT &&
                            (event.getMetaState() & KeyEvent.META_SHIFT_ON) != 0) {
                            hostContextClick = true;
                            showClipboardMenu(event.getX(), event.getY());
                            return true;
                        }
                        if (mouseButton != NativeSession.BUTTON_NONE && mouseButton != button) return true;
                        if (mouseButton == NativeSession.BUTTON_NONE) mouseButton = button;
                        mouseEvent(event, button, NativeSession.POINTER_PRESS);
                    } else {
                        if (button == NativeSession.BUTTON_RIGHT && hostContextClick) {
                            hostContextClick = false;
                            return true;
                        }
                        if (mouseButton == button) {
                            mouseEvent(event, button, NativeSession.POINTER_RELEASE);
                            mouseButton = NativeSession.BUTTON_NONE;
                        }
                    }
                    return true;
                default:
                    return super.onGenericMotionEvent(event);
            }
        }
        /** Take the gesture away from scrolling: it belongs to the clipboard now. */
        private void beginSelection(MotionEvent down) {
            requestFocus();
            selecting = true; dragging = false;
            pressX = down.getX(); pressY = down.getY();
            performHapticFeedback(HapticFeedbackConstants.LONG_PRESS);
            composition.setText("Drag to select · lift for the clipboard menu");
            MotionEvent cancel = MotionEvent.obtain(down);
            cancel.setAction(MotionEvent.ACTION_CANCEL);
            gestures.onTouchEvent(cancel);
            cancel.recycle();
        }
        private boolean dragSelection(MotionEvent event) {
            float x = event.getX(), y = event.getY();
            int action = event.getActionMasked();
            if (action == MotionEvent.ACTION_MOVE) {
                if (!dragging) {
                    if (Math.hypot(x - pressX, y - pressY) < slop) return true;
                    dragging = true;
                    final float fromX = pressX, fromY = pressY;
                    session(h -> NativeSession.select(h, fromX, fromY, 0));
                }
                session(h -> NativeSession.select(h, x, y, 1));
            } else if (action == MotionEvent.ACTION_UP) {
                endSelection(x, y, true);
            } else if (action == MotionEvent.ACTION_CANCEL || action == MotionEvent.ACTION_POINTER_DOWN) {
                endSelection(x, y, false);
            }
            return true;
        }
        /** A drag that ends on the text copies it; a hold that never moved asks. */
        private void endSelection(float x, float y, boolean lifted) {
            selecting = false;
            composition.setText("");
            if (dragging) {
                session(h -> NativeSession.select(h, x, y, 2));
                if (lifted) copySelection();
            } else if (lifted) {
                showClipboardMenu(x, y);
            }
            dragging = false;
        }
        @Override public boolean onCheckIsTextEditor() { return true; }
        @Override public InputConnection onCreateInputConnection(EditorInfo info) {
            info.inputType = InputType.TYPE_CLASS_TEXT | InputType.TYPE_TEXT_FLAG_MULTI_LINE | InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS;
            info.imeOptions = EditorInfo.IME_FLAG_NO_EXTRACT_UI | EditorInfo.IME_FLAG_NO_FULLSCREEN | EditorInfo.IME_ACTION_NONE;
            info.initialSelStart = info.initialSelEnd = 0;
            return new BaseInputConnection(this, true) {
                private final Editable editable = new SpannableStringBuilder();
                // How much of the composition in progress the pane already
                // has, which is nothing unless it is going straight through.
                private String typed = "";
                // Decided once per composition: asking the keyboard what
                // language it is in the middle of one would answer about the
                // keyboard the user has since switched to.
                private boolean direct;
                @Override public Editable getEditable() { return editable; }
                private void clear() { editable.clear(); removeComposingSpans(editable); composition.setText(""); }
                /** Send the difference between what the pane has and what the keyboard now says. */
                private void typeThrough(String text) {
                    int same = 0;
                    while (same < typed.length() && same < text.length() && typed.charAt(same) == text.charAt(same)) same++;
                    for (int i = typed.length(); i > same; i--) backspace();
                    if (text.length() > same) commit(text.substring(same));
                    typed = text;
                }
                /** Take back what the keyboard has since changed its mind about. */
                private void retract() { typeThrough(""); typed = ""; }
                @Override public boolean setComposingText(CharSequence text, int cursor) {
                    if (typed.isEmpty() && editable.length() == 0) direct = directKeyboard();
                    if (direct && ascii(text)) {
                        if (editable.length() > 0) clear();
                        typeThrough(text.toString());
                        return true;
                    }
                    retract();
                    super.setComposingText(text, cursor); composition.setText(editable); return true;
                }
                @Override public boolean commitText(CharSequence text, int cursor) {
                    if (!typed.isEmpty()) {
                        if (ascii(text)) { typeThrough(text.toString()); typed = ""; clear(); return true; }
                        // A keyboard that commits a newline or an emoji on top
                        // of what it already typed through is adding to the
                        // line, not replacing it: taking the line back here
                        // would erase a command the user can see and meant.
                        typed = "";
                    }
                    commit(text.toString()); clear(); return true;
                }
                @Override public boolean finishComposingText() {
                    // A composition that went straight through is already in
                    // the pane; committing it again would type it twice.
                    if (!typed.isEmpty()) { typed = ""; clear(); return true; }
                    if (editable.length() > 0) commit(editable.toString());
                    clear(); return true;
                }
                @Override public boolean deleteSurroundingText(int before, int after) {
                    if (!typed.isEmpty()) typed = typed.substring(0, Math.max(0, typed.length() - before));
                    if (editable.length() > 0) { super.deleteSurroundingText(before, after); composition.setText(editable); }
                    else {
                        for (int i = 0; i < Math.min(before, 1024); i++) special(KeyEvent.KEYCODE_DEL);
                        for (int i = 0; i < Math.min(after, 1024); i++) special(KeyEvent.KEYCODE_FORWARD_DEL);
                    }
                    return true;
                }
                @Override public boolean deleteSurroundingTextInCodePoints(int before, int after) {
                    if (typed.isEmpty() && editable.length() > 0) {
                        super.deleteSurroundingTextInCodePoints(before, after); composition.setText(editable); return true;
                    }
                    return deleteSurroundingText(before, after);
                }
                @Override public boolean sendKeyEvent(KeyEvent e) { typed = ""; return handleKey(e); }
                @Override public boolean performEditorAction(int action) {
                    typed = ""; special(KeyEvent.KEYCODE_ENTER); return true;
                }
            };
        }
        private boolean handleKey(KeyEvent event) {
            int code = event.getKeyCode();
            if (event.isCtrlPressed() && event.isShiftPressed()) {
                if (code == KeyEvent.KEYCODE_C || code == KeyEvent.KEYCODE_V) {
                    if (event.getAction() == KeyEvent.ACTION_DOWN && event.getRepeatCount() == 0) {
                        if (code == KeyEvent.KEYCODE_C) copySelection(); else paste();
                    }
                    return true;
                }
            }
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
