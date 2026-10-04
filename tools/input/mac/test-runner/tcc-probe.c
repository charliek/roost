// Reports (and with `request`, asks for) the TCC grants the Mac real-input
// harness needs. Run it as a child of Roost Test Runner so the grants it
// reports are the runner's.
#include <ApplicationServices/ApplicationServices.h>
#include <IOKit/hidsystem/IOHIDLib.h>
#include <stdio.h>
#include <string.h>

int main(int argc, char **argv) {
    if (argc > 1 && strcmp(argv[1], "request") == 0) {
        const void *keys[] = {kAXTrustedCheckOptionPrompt};
        const void *values[] = {kCFBooleanTrue};
        CFDictionaryRef options = CFDictionaryCreate(NULL, keys, values, 1,
            &kCFCopyStringDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
        AXIsProcessTrustedWithOptions(options);
        CFRelease(options);
        CGRequestPostEventAccess();
        CGRequestScreenCaptureAccess();
        IOHIDRequestAccess(kIOHIDRequestTypeListenEvent);
    }
    printf("accessibility=%d post_event=%d screen_capture=%d input_monitoring=%d\n",
        AXIsProcessTrusted(), CGPreflightPostEventAccess(), CGPreflightScreenCaptureAccess(),
        IOHIDCheckAccess(kIOHIDRequestTypeListenEvent) == kIOHIDAccessTypeGranted);
    return 0;
}
