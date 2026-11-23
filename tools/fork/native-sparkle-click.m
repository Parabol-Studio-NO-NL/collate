// Test-only AppKit input for the disposable updater fixture on a macOS runner.
#import <AppKit/AppKit.h>
#import <dispatch/dispatch.h>

#include <stdio.h>
#include <stdlib.h>

static BOOL IsRunnerCollate(void)
{
    NSBundle *app = NSBundle.mainBundle;
    NSString *runnerTemp = NSProcessInfo.processInfo.environment[@"RUNNER_TEMP"];
    if (runnerTemp.length == 0 || ![app.bundleIdentifier isEqualToString:@"com.larshoie.codesign"]) {
        return NO;
    }
    NSURL *root = [NSURL fileURLWithPath:runnerTemp isDirectory:YES].URLByResolvingSymlinksInPath;
    NSURL *bundle = app.bundleURL.URLByResolvingSymlinksInPath;
    return [bundle.lastPathComponent isEqualToString:@"Collate.app"]
        && [bundle.path hasPrefix:[root.path stringByAppendingString:@"/"]];
}

static BOOL Log(NSString *path, NSString *line)
{
    FILE *file = fopen(path.fileSystemRepresentation, "a");
    if (file == NULL) {
        return NO;
    }
    BOOL written = fprintf(file, "%s\n", line.UTF8String) >= 0;
    return fclose(file) == 0 && written;
}

static NSButton *FindButton(NSView *view, Class targetClass, SEL action, NSArray<NSString *> *titles)
{
    if (view.isHiddenOrHasHiddenAncestor || NSIsEmptyRect(view.visibleRect)) {
        return nil;
    }
    if ([view isKindOfClass:NSButton.class]) {
        NSButton *button = (NSButton *)view;
        if (button.isEnabled && button.action == action && [button.target isMemberOfClass:targetClass]
            && [titles containsObject:button.title]) {
            return button;
        }
    }
    for (NSView *child in view.subviews) {
        NSButton *button = FindButton(child, targetClass, action, titles);
        if (button != nil) {
            return button;
        }
    }
    return nil;
}

static NSButton *InstallButton(NSWindow *window)
{
    if (!window.isVisible || window.isMiniaturized) {
        return nil;
    }

    // These window identities and selectors are from the pinned Sparkle 2.10.1 resources.
    BOOL isUpdateAlert = [window.identifier isEqualToString:@"SUUpdateAlert"];
    if (!isUpdateAlert && ![window.identifier isEqualToString:@"SUStatus"]) {
        return nil;
    }
    Class controller = NSClassFromString(isUpdateAlert ? @"SUUpdateAlert" : @"SUStatusController");
    if (controller == Nil || ![window.windowController isMemberOfClass:controller]) {
        return nil;
    }
    NSBundle *sparkle = [NSBundle bundleForClass:controller];
    if (![sparkle.bundleIdentifier isEqualToString:@"org.sparkle-project.Sparkle"]
        || ![sparkle.infoDictionary[@"CFBundleShortVersionString"] isEqual:@"2.10.1"]) {
        return nil;
    }

    NSString *relaunch = [sparkle localizedStringForKey:@"Install and Relaunch" value:@"Install and Relaunch" table:@"Sparkle"];
    if (isUpdateAlert) {
        NSString *install = [sparkle localizedStringForKey:@"Install Update" value:@"Install Update" table:@"Sparkle"];
        return FindButton(window.contentView, controller, NSSelectorFromString(@"installUpdate:"), @[install, relaunch]);
    }
    Class driver = NSClassFromString(@"SPUStandardUserDriver");
    NSButton *install = FindButton(window.contentView, driver, NSSelectorFromString(@"installAndRestart:"), @[relaunch]);
    return install ?: FindButton(window.contentView, driver, NSSelectorFromString(@"retryTermination:"), @[relaunch]);
}

__attribute__((constructor)) static void StartSparkleClickObserver(void)
{
    if (getenv("COLLATE_SPARKLE_CLICK_LOG") == NULL) {
        return;
    }
    dispatch_async(dispatch_get_main_queue(), ^{
        NSString *path = NSProcessInfo.processInfo.environment[@"COLLATE_SPARKLE_CLICK_LOG"];
        if (path.length == 0 || !IsRunnerCollate() || !Log(path, @"observer-ready")) {
            return;
        }

        // Wait for Sparkle's windows to appear on the main AppKit run loop. Remember each
        // clicked button even if a nested native event loop runs while handling the action.
        NSHashTable<NSButton *> *clicked = [NSHashTable weakObjectsHashTable];
        NSTimer *timer = [NSTimer timerWithTimeInterval:0.2 repeats:YES block:^(NSTimer *tick) {
            (void)tick;
            for (NSWindow *window in NSApp.windows) {
                NSButton *button = InstallButton(window);
                if (button == nil || [clicked containsObject:button]) {
                    continue;
                }
                NSString *event = [NSString stringWithFormat:@"clicked %@ %@", NSStringFromSelector(button.action), button.title];
                if (Log(path, event)) {
                    [clicked addObject:button];
                    [button performClick:nil];
                    return;
                }
            }
        }];
        [NSRunLoop.mainRunLoop addTimer:timer forMode:NSRunLoopCommonModes];
        [NSRunLoop.mainRunLoop addTimer:timer forMode:NSModalPanelRunLoopMode];
    });
}
