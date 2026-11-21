package com.autokj.studio;

import android.app.Activity;
import android.content.Intent;
import android.net.Uri;
import android.os.Bundle;
import android.os.Build;
import android.view.View;
import android.view.WindowInsets;
import android.security.keystore.KeyGenParameterSpec;
import android.security.keystore.KeyProperties;
import android.util.Base64;
import android.webkit.JavascriptInterface;
import android.webkit.JsResult;
import android.webkit.WebChromeClient;
import android.webkit.WebResourceRequest;
import android.webkit.WebResourceResponse;
import android.webkit.WebView;
import android.webkit.WebViewClient;
import android.widget.*;
import org.json.JSONObject;

import javax.crypto.*;
import javax.crypto.spec.GCMParameterSpec;
import javax.net.ssl.*;
import java.io.*;
import java.net.URL;
import java.nio.charset.StandardCharsets;
import java.security.KeyStore;
import java.security.MessageDigest;
import java.security.cert.*;
import java.util.*;
import java.util.concurrent.*;

/** Hosts only packaged UI. Every venue-network request checks the host certificate from the pairing QR. */
public class StudioActivity extends Activity {
    private static final String UI = "https://studio.invalid/";
    // A request must start now or report busy. Never replay accumulated fader
    // moves when a slow TLS connection or a sleeping tablet becomes available.
    private static ExecutorService immediateWorkers(int limit) {
        return new ThreadPoolExecutor(0,limit,30,TimeUnit.SECONDS,new SynchronousQueue<>());
    }
    private final ExecutorService reads = immediateWorkers(2);
    private final ExecutorService commands = immediateWorkers(4);
    private volatile WebView desk;
    private volatile long foregroundGeneration;
    private volatile boolean foreground;
    private FrameLayout content;
    private boolean canReturnToPair;

    @Override public void onCreate(Bundle saved) {
        super.onCreate(saved);
        content=new FrameLayout(this);
        if(Build.VERSION.SDK_INT>=30){
            getWindow().setDecorFitsSystemWindows(false);
            content.setOnApplyWindowInsetsListener((view,insets)->{
                android.graphics.Insets safe=insets.getInsets(WindowInsets.Type.systemBars()
                    | WindowInsets.Type.displayCutout() | WindowInsets.Type.ime());
                view.setPadding(safe.left,safe.top,safe.right,safe.bottom);
                return WindowInsets.CONSUMED;
            });
        }
        setContentView(content);
        Uri incoming = getIntent().getData();
        if (incoming != null) { showPairingLink(incoming); return; }
        if (!restorePair()) pairingForm("");
    }

    @Override protected void onNewIntent(Intent intent) {
        super.onNewIntent(intent);
        Uri incoming = intent.getData();
        if (incoming != null) {
            setIntent(intent);
            showPairingLink(incoming);
        }
    }

    private void showPairingLink(Uri incoming) {
        closeDesk();
        canReturnToPair = true;
        pairingForm(incoming.toString());
    }

    private boolean restorePair() {
        try {
            String secret = getPreferences(0).getString("station", null);
            if (secret != null) {
                open(new Pair(decrypt(secret)));
                return true;
            }
        } catch (Exception ignored) { /* Allow re-pair after device key changes. */ }
        return false;
    }

    private void closeDesk() {
        foregroundGeneration++;
        WebView previous = desk;
        desk = null;
        if (previous != null) {
            content.removeView(previous);
            previous.removeJavascriptInterface("StudioTablet");
            previous.stopLoading();
            previous.destroy();
        }
    }

    private void pairingForm(String value) {
        LinearLayout form = new LinearLayout(this);
        form.setOrientation(LinearLayout.VERTICAL); form.setPadding(36,36,36,36);
        TextView title = new TextView(this); title.setText("Pair Auto-KJ Studio Console"); title.setTextSize(24);
        TextView help = new TextView(this); help.setText("Scan the operator QR from Auto-KJ Settings, or paste the private pairing link.");
        EditText link = new EditText(this); link.setSingleLine(); link.setHint("Operator pairing link");
        link.setInputType(android.text.InputType.TYPE_CLASS_TEXT | android.text.InputType.TYPE_TEXT_VARIATION_PASSWORD);
        link.setText(value);
        Button pair = new Button(this); pair.setText("Pair with show host");
        pair.setOnClickListener(v -> {
            String raw = link.getText().toString().trim();
            final Pair binding;
            try {
                binding = new Pair(raw);
            } catch (Exception invalid) {
                help.setText("Use the current operator pairing link from Auto-KJ Settings.");
                return;
            }
            long generation = foregroundGeneration;
            pair.setEnabled(false);
            link.setEnabled(false);
            help.setText("Checking the show host...");
            try {
                commands.execute(() -> {
                    String error = verifyPair(binding);
                    runOnUiThread(() -> {
                        if (!form.isAttachedToWindow() || isFinishing() || isDestroyed()) return;
                        pair.setEnabled(true);
                        link.setEnabled(true);
                        if (!foreground || foregroundGeneration != generation) {
                            help.setText("Pairing interrupted. Tap Pair with show host to try again.");
                        } else if (error != null) {
                            help.setText(error);
                        } else {
                            try {
                                getPreferences(0).edit().putString("station", encrypt(raw)).apply();
                                open(binding);
                            } catch (Exception storageError) {
                                help.setText("Could not save the pairing on this tablet. Try again.");
                            }
                        }
                    });
                });
            } catch (RejectedExecutionException busy) {
                pair.setEnabled(true);
                link.setEnabled(true);
                help.setText("The tablet is busy. Try pairing again.");
            }
        });
        form.addView(title);form.addView(help);form.addView(link);form.addView(pair);
        ScrollView scroll=new ScrollView(this);
        scroll.addView(form);
        showContent(scroll);
    }

    private static String verifyPair(Pair binding) {
        HttpsURLConnection connection = null;
        try {
            connection = binding.connect("state", 3500);
            int status = connection.getResponseCode();
            if (status == 401 || status == 403)
                return "The show host rejected this pairing. Scan a new operator QR.";
            if (status != 200) return "The show host could not confirm this pairing. Try again.";
            JSONObject state = new JSONObject(responseText(connection.getInputStream()));
            state.getBoolean("armed");
            state.getJSONObject("mixer");
            state.getJSONObject("playback");
            return null;
        } catch (SSLException invalidCertificate) {
            return "Could not verify the show host certificate. Scan its current operator QR.";
        } catch (Exception unavailable) {
            return "Could not verify the show host. Check the host address and Wi-Fi, then try again.";
        } finally {
            if (connection != null) connection.disconnect();
        }
    }

    private static String responseText(InputStream stream) throws IOException {
        if (stream == null) return "";
        try (InputStream in = stream; ByteArrayOutputStream bytes = new ByteArrayOutputStream()) {
            byte[] buffer = new byte[8192];
            int count;
            while ((count = in.read(buffer)) != -1) {
                if (bytes.size() + count > 1024 * 1024) throw new IOException("State exceeds size limit");
                bytes.write(buffer, 0, count);
            }
            return bytes.toString("UTF-8");
        }
    }

    private void showContent(View view) {
        content.removeAllViews();
        content.addView(view,FrameLayout.LayoutParams.MATCH_PARENT,FrameLayout.LayoutParams.MATCH_PARENT);
    }

    static class Pair {
        final URL origin;
        final String token;
        final byte[] pin;
        final SSLSocketFactory sockets;
        Pair(String link) throws Exception {
            Uri uri=Uri.parse(link);
            if ("autokj-studio".equals(uri.getScheme()) && "pair".equals(uri.getHost())) {
                uri=Uri.parse("https://"+uri.getQueryParameter("address")+"/#"+uri.getEncodedFragment());
            }
            if (!"https".equals(uri.getScheme()) || uri.getPort()<1 || uri.getPort()>65535 ||
                uri.getUserInfo()!=null || !"/".equals(uri.getPath()) || uri.getHost()==null ||
                !uri.getHost().matches("[0-9]{1,3}(\\.[0-9]{1,3}){3}")) throw new IllegalArgumentException();
            for(String octet:uri.getHost().split("\\.")) if(Integer.parseInt(octet)>255) throw new IllegalArgumentException();
            Uri params=Uri.parse("https://studio.invalid/?"+uri.getEncodedFragment());
            token=UUID.fromString(params.getQueryParameter("token")).toString();
            String hash=params.getQueryParameter("pin");
            if(hash==null || !hash.matches("[a-fA-F0-9]{64}")) throw new IllegalArgumentException();
            pin=new byte[32]; for(int i=0;i<32;i++) pin[i]=(byte)Integer.parseInt(hash.substring(2*i,2*i+2),16);
            origin=new URL("https",uri.getHost(),uri.getPort(),"/");
            X509TrustManager trust=new X509TrustManager() {
                public X509Certificate[] getAcceptedIssuers(){ return new X509Certificate[0]; }
                public void checkClientTrusted(X509Certificate[] c,String t) throws CertificateException {throw new CertificateException();}
                public void checkServerTrusted(X509Certificate[] chain,String type) throws CertificateException {
                    if(chain==null || chain.length==0) throw new CertificateException("Missing host certificate");
                    chain[0].checkValidity();
                    try {
                        if (!MessageDigest.isEqual(pin,MessageDigest.getInstance("SHA-256").digest(chain[0].getEncoded())))
                            throw new CertificateException("Host certificate changed. Pair again at the booth.");
                    } catch(java.security.NoSuchAlgorithmException e) {throw new CertificateException(e);}
                }
            };
            SSLContext tls=SSLContext.getInstance("TLS");tls.init(null,new TrustManager[]{trust},null);sockets=tls.getSocketFactory();
        }

        HttpsURLConnection connect(String path, int readTimeout) throws IOException {
            HttpsURLConnection connection = (HttpsURLConnection)new URL(origin, "api/" + path).openConnection();
            connection.setSSLSocketFactory(sockets);
            connection.setInstanceFollowRedirects(false);
            connection.setConnectTimeout(2000);
            connection.setReadTimeout(readTimeout);
            connection.setRequestProperty("Authorization", "Bearer " + token);
            return connection;
        }
    }

    private void open(Pair binding) {
        canReturnToPair = false;
        getIntent().setData(null);
        WebView web=new WebView(this);desk=web;showContent(web);
        web.getSettings().setJavaScriptEnabled(true);
        web.getSettings().setDomStorageEnabled(true);
        web.getSettings().setAllowFileAccess(false);web.getSettings().setAllowContentAccess(false);
        web.setWebChromeClient(new WebChromeClient(){
            @Override public boolean onJsConfirm(WebView view,String url,String message,JsResult result){
                if(!UI.equals(url) || isFinishing()){result.cancel();return true;}
                new android.app.AlertDialog.Builder(StudioActivity.this).setMessage(message)
                    .setNegativeButton("Cancel",(dialog,which)->result.cancel())
                    .setPositiveButton("Confirm",(dialog,which)->result.confirm())
                    .setOnCancelListener(dialog->result.cancel()).show();
                return true;
            }
        });
        web.setWebViewClient(new WebViewClient(){
            @Override public boolean shouldOverrideUrlLoading(WebView w,WebResourceRequest r){return true;}
            @Override public WebResourceResponse shouldInterceptRequest(WebView w,WebResourceRequest r) {
                String url=r.getUrl().toString();
                String asset=url.equals(UI)?"index.html":url.equals(UI+"console.js")?"console.js":url.equals(UI+"operator.js")?"operator.js":url.equals(UI+"console.css")?"console.css":null;
                try {
                    if(asset!=null) return new WebResourceResponse(asset.endsWith(".js")?"text/javascript":asset.endsWith(".css")?"text/css":"text/html","utf-8",getAssets().open(asset));
                }catch(IOException ignored){}
                return new WebResourceResponse("text/plain","utf-8",403,"Forbidden",Collections.emptyMap(),new ByteArrayInputStream(new byte[0]));
            }
        });
        web.addJavascriptInterface(new Connection(binding,web),"StudioTablet");
        web.loadUrl(UI);
    }

    private final class Connection {
        private final Pair pair;
        private final WebView owner;
        Connection(Pair pair,WebView owner){this.pair=pair;this.owner=owner;}
        private void reply(int id,int status,String answer) {
            String callback="window.studioReply("+id+","+status+","+JSONObject.quote(answer)+")";
            runOnUiThread(()->{if(desk==owner) owner.evaluateJavascript(callback,null);});
        }
        @JavascriptInterface public void request(int id,String path,String body) {
            if(id<0)return;
            if(!Arrays.asList("state","sync","meters","arm","mix","send","show","fx","operator","preamp").contains(path) || body==null || body.length()>4096) {
                reply(id,400,"Invalid console request");return;
            }
            final long generation=foregroundGeneration;
            boolean polling=path.equals("sync") || path.equals("state") || path.equals("meters");
            final Runnable send=()->{
                if(!foreground || foregroundGeneration!=generation || desk!=owner) {
                    reply(id,409,"The tablet went to sleep. Refresh the console.");return;
                }
                HttpsURLConnection con=null;
                int status=503;
                String answer="Could not reach the paired Auto-KJ host";
                try {
                    con=pair.connect(path, path.equals("operator") || path.equals("show") ? 30000 : 3500);
                    if(!body.isEmpty()){
                        con.setRequestMethod("POST");con.setDoOutput(true);
                        con.setRequestProperty("Content-Type","application/json");
                        byte[] bytes=body.getBytes(StandardCharsets.UTF_8);
                        con.setFixedLengthStreamingMode(bytes.length);
                        try(OutputStream out=con.getOutputStream()){
                            if(!foreground || foregroundGeneration!=generation || desk!=owner) {
                                throw new CancellationException("The tablet went to sleep. Refresh the console.");
                            }
                            out.write(bytes);
                        }
                    }
                    status=con.getResponseCode();
                    InputStream stream=status>=400?con.getErrorStream():con.getInputStream();
                    answer=responseText(stream);
                }catch(CancellationException e){status=409;answer=e.getMessage();}
                catch(Exception e){status=503;if(!body.isEmpty() && !path.equals("sync"))answer="No confirmation received. Refresh the show state before repeating the action.";}
                finally{if(con!=null)con.disconnect();}
                reply(id,status,answer);
            };
            try { (polling?reads:commands).execute(send); }
            catch(RejectedExecutionException busy) { reply(id,429,"The tablet is busy. Wait for the current controls to finish."); }
        }
    }

    @Override protected void onStart(){super.onStart();foreground=true;}
    @Override protected void onStop(){foreground=false;foregroundGeneration++;super.onStop();}

    @Override public void onBackPressed() {
        if (desk == null) {
            if (canReturnToPair && restorePair()) return;
            super.onBackPressed();
            return;
        }
        new android.app.AlertDialog.Builder(this).setMessage("Return to operator pairing?")
            .setNegativeButton("Stay on console",null)
            .setNeutralButton("Forget host",(d,w)->{
                getPreferences(0).edit().remove("station").apply();
                canReturnToPair=false;
                closeDesk();pairingForm("");
            })
            .setPositiveButton("Pair another host",(d,w)->{
                canReturnToPair=true;
                closeDesk();pairingForm("");
            }).show();
    }
    @Override protected void onDestroy(){
        closeDesk();reads.shutdownNow();commands.shutdownNow();super.onDestroy();
    }

    private SecretKey deviceKey() throws Exception {
        String alias="autokj-studio-operator";
        KeyStore keys=KeyStore.getInstance("AndroidKeyStore");keys.load(null);
        if(keys.containsAlias(alias))return (SecretKey)keys.getKey(alias,null);
        KeyGenerator generator=KeyGenerator.getInstance("AES","AndroidKeyStore");
        generator.init(new KeyGenParameterSpec.Builder(alias,KeyProperties.PURPOSE_ENCRYPT|KeyProperties.PURPOSE_DECRYPT)
            .setBlockModes(KeyProperties.BLOCK_MODE_GCM).setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE).build());
        return generator.generateKey();
    }
    private String encrypt(String text) throws Exception {
        Cipher aes=Cipher.getInstance("AES/GCM/NoPadding");aes.init(Cipher.ENCRYPT_MODE,deviceKey());
        return Base64.encodeToString(aes.getIV(),Base64.NO_WRAP)+"."+Base64.encodeToString(aes.doFinal(text.getBytes(StandardCharsets.UTF_8)),Base64.NO_WRAP);
    }
    private String decrypt(String text) throws Exception {
        String[] parts=text.split("\\.",2);
        Cipher aes=Cipher.getInstance("AES/GCM/NoPadding");
        aes.init(Cipher.DECRYPT_MODE,deviceKey(),new GCMParameterSpec(128,Base64.decode(parts[0],Base64.NO_WRAP)));
        return new String(aes.doFinal(Base64.decode(parts[1],Base64.NO_WRAP)),StandardCharsets.UTF_8);
    }
}
