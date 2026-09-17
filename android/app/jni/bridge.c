#include <jni.h>
#include <android/native_window_jni.h>
#include <stdint.h>
#include <stdbool.h>
#include <stdlib.h>

typedef struct Engine Engine;
extern Engine *tos_android_create(const char *, uint32_t, uint32_t, float);
extern int32_t tos_android_tick(Engine *);
extern bool tos_android_render(Engine *, uint32_t *, uint32_t, uint32_t, uint32_t);
extern void tos_android_resize(Engine *, uint32_t, uint32_t, float);
extern void tos_android_text(Engine *, const uint16_t *, size_t, uint8_t, bool);
extern void tos_android_key(Engine *, uint32_t, uint32_t, uint8_t, bool);
extern void tos_android_pointer(Engine *, double, double, int32_t);
extern void tos_android_action(Engine *, uint32_t);
extern const uint8_t *tos_android_clipboard(Engine *, size_t *);
extern uint64_t tos_android_grid(Engine *);
extern void tos_android_destroy(Engine *);

typedef struct { Engine *engine; ANativeWindow *window; bool redraw; } Client;
#define JNI_METHOD(name) Java_io_github_m96chan_tos_NativeSession_##name
#define CLIENT ((Client *)(intptr_t)handle)

JNIEXPORT jlong JNICALL JNI_METHOD(create)(JNIEnv *env, jclass cls, jstring home, jfloat font) {
    (void)cls;
    const char *path = (*env)->GetStringUTFChars(env, home, NULL);
    if (!path) return 0;
    Client *client = calloc(1, sizeof(Client));
    if (client) client->engine = tos_android_create(path, 640, 480, font);
    (*env)->ReleaseStringUTFChars(env, home, path);
    if (!client || !client->engine) { free(client); return 0; }
    return (jlong)(intptr_t)client;
}

JNIEXPORT void JNICALL JNI_METHOD(surface)(JNIEnv *env, jclass cls, jlong handle, jobject surface) {
    (void)cls;
    Client *c = CLIENT;
    if (c->window) ANativeWindow_release(c->window);
    c->window = surface ? ANativeWindow_fromSurface(env, surface) : NULL;
    c->redraw = true;
    if (c->window) ANativeWindow_setBuffersGeometry(c->window, 0, 0, WINDOW_FORMAT_RGBA_8888);
}

JNIEXPORT jint JNICALL JNI_METHOD(step)(JNIEnv *env, jclass cls, jlong handle) {
    (void)env; (void)cls;
    Client *c = CLIENT;
    int32_t state = tos_android_tick(c->engine);
    if (state < 0) return -1;
    if (!c->window || (!state && !c->redraw)) return 0;
    ANativeWindow_Buffer buffer;
    if (ANativeWindow_lock(c->window, &buffer, NULL) != 0) return 0;
    bool ok = tos_android_render(c->engine, buffer.bits, buffer.width, buffer.height, buffer.stride);
    ANativeWindow_unlockAndPost(c->window);
    c->redraw = !ok;
    return ok ? 1 : 0;
}

JNIEXPORT void JNICALL JNI_METHOD(resize)(JNIEnv *env, jclass cls, jlong handle, jint width, jint height, jfloat font) {
    (void)env; (void)cls;
    tos_android_resize(CLIENT->engine, width, height, font);
    CLIENT->redraw = true;
}

JNIEXPORT void JNICALL JNI_METHOD(text)(JNIEnv *env, jclass cls, jlong handle, jstring text, jint modifiers, jboolean paste) {
    (void)cls;
    const jchar *chars = (*env)->GetStringChars(env, text, NULL);
    if (!chars) return;
    tos_android_text(CLIENT->engine, chars, (*env)->GetStringLength(env, text), modifiers, paste);
    (*env)->ReleaseStringChars(env, text, chars);
}

JNIEXPORT void JNICALL JNI_METHOD(key)(JNIEnv *env, jclass cls, jlong handle, jint code, jint unicode, jint modifiers, jboolean release) {
    (void)env; (void)cls;
    tos_android_key(CLIENT->engine, code, unicode, modifiers, release);
}

JNIEXPORT void JNICALL JNI_METHOD(pointer)(JNIEnv *env, jclass cls, jlong handle, jfloat x, jfloat y, jint wheel) {
    (void)env; (void)cls;
    tos_android_pointer(CLIENT->engine, x, y, wheel);
}

JNIEXPORT void JNICALL JNI_METHOD(action)(JNIEnv *env, jclass cls, jlong handle, jint action) {
    (void)env; (void)cls;
    tos_android_action(CLIENT->engine, action);
}

JNIEXPORT jbyteArray JNICALL JNI_METHOD(clipboard)(JNIEnv *env, jclass cls, jlong handle) {
    (void)cls;
    size_t length = 0;
    const uint8_t *data = tos_android_clipboard(CLIENT->engine, &length);
    if (length > 4194304) return NULL;
    jbyteArray bytes = (*env)->NewByteArray(env, length);
    if (bytes && length) (*env)->SetByteArrayRegion(env, bytes, 0, length, (const jbyte *)data);
    return bytes;
}

JNIEXPORT jlong JNICALL JNI_METHOD(grid)(JNIEnv *env, jclass cls, jlong handle) {
    (void)env; (void)cls;
    return (jlong)tos_android_grid(CLIENT->engine);
}

JNIEXPORT void JNICALL JNI_METHOD(destroy)(JNIEnv *env, jclass cls, jlong handle) {
    (void)env; (void)cls;
    Client *c = CLIENT;
    if (!c) return;
    if (c->window) ANativeWindow_release(c->window);
    tos_android_destroy(c->engine);
    free(c);
}
