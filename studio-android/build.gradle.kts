plugins { id("com.android.application") version "8.9.2" }

val deskAssets by tasks.registering(Sync::class) {
    from("../local-server/src/studio") { include("*.html", "*.js", "*.css") }
    into(layout.buildDirectory.dir("generated/studioAssets"))
}
tasks.named("preBuild") { dependsOn(deskAssets) }

android {
    namespace = "com.autokj.studio"
    compileSdk = 35
    defaultConfig {
        applicationId = "com.autokj.studio"
        minSdk = 28
        targetSdk = 35
        versionCode = 1
        versionName = "0.1"
    }
    sourceSets.getByName("main").assets.srcDir(layout.buildDirectory.dir("generated/studioAssets"))
}
