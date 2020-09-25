import static org.junit.Assert.*;
import org.junit.Test;

public class TestStringGenerator {
    @Test
    public void testStringGenerator() {
        StringGenerator generator = new StringGenerator();
        assertTrue(generator.createString().equals("AString"));
    }
}
