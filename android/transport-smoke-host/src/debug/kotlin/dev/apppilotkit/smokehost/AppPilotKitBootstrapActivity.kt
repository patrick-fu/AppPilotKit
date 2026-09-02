package dev.apppilotkit.smokehost

import android.app.Activity
import android.os.Bundle
import dev.apppilotkit.targettransport.internal.TargetTransport
import dev.apppilotkit.targettransport.internal.TargetTransportBootstrap

class AppPilotKitBootstrapActivity : Activity() {
    private var transport: TargetTransport? = null

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        try {
            transport = TargetTransportBootstrap.start(this)
        } catch (_: Throwable) {
            finish()
        }
    }

    override fun onPause() {
        transport?.eligibilityLost()
        super.onPause()
    }

    override fun onDestroy() {
        val current = transport
        transport = null
        if (current != null) {
            current.close()
            TargetTransportBootstrap.clear(current)
        }
        super.onDestroy()
    }
}
