package dev.jarexec.examples.hello;

import org.apache.commons.text.WordUtils;
import org.slf4j.Logger;
import org.slf4j.LoggerFactory;

public class Main {
    private static final Logger LOG = LoggerFactory.getLogger(Main.class);

    public static void main(String[] args) {
        // used by jarexec tests to check that the exit code is forwarded
        if (args.length == 2 && args[0].equals("--exit")) {
            System.exit(Integer.parseInt(args[1]));
        }
        String who = args.length > 0 ? String.join(" ", args) : "jarexec";
        LOG.info("Java {} from {}", System.getProperty("java.version"), System.getProperty("java.home"));
        System.out.println(WordUtils.capitalizeFully("hello, " + who + "!"));
    }
}
