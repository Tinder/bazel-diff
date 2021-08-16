package integration.test.java.com.integration;

import static org.junit.Assert.*;

import integration.src.main.java.com.integration.StringGenerator;
import org.junit.Test;

public class TestStringGenerator {
    @Test
    public void testStringGenerator() {
        StringGenerator generator = new StringGenerator();
        assertTrue(generator.createString().equals("AString"));
    }
}
